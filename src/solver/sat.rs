//! `Solver` and `RuleSetGenerator`.

use super::policy::DefaultPolicy;
use super::problem::Problem;
use super::rules::{
    Decisions, Reason, Rule, RuleSet, RuleWatchGraph, TYPE_LEARNED, TYPE_PACKAGE, TYPE_REQUEST,
};
use super::{Literal, Pool, Request};
use crate::io::Io;
use crate::platform::RequirementFilter;
use crate::semver::Constraint;
use std::collections::{HashMap, HashSet, VecDeque};

pub const ERROR_DEPENDENCY_RESOLUTION_FAILED: i32 = 2;

/// `SolverProblemsException`
pub struct SolverProblems {
    pub problems: Vec<Problem>,
    pub learned_pool: Vec<Vec<usize>>,
}

/// `RuleSetGenerator::getRulesFor`
pub fn generate_rules(pool: &Pool, request: &Request, filter: &RequirementFilter) -> RuleSet {
    let mut generator = Generator {
        pool,
        rules: RuleSet::default(),
        added: HashSet::new(),
        added_order: Vec::new(),
        added_by_name: Vec::new(),
    };
    generator.add_rules_for_request(request, filter);
    generator.add_rules_for_root_aliases(filter);
    generator.add_conflict_rules(filter);
    generator.rules
}

struct Generator<'a> {
    pool: &'a Pool,
    rules: RuleSet,
    added: HashSet<usize>,
    added_order: Vec<usize>,
    /// name => package indices, in first-seen order
    added_by_name: Vec<(String, Vec<usize>)>,
}

impl Generator<'_> {
    fn add(&mut self, kind: u8, rule: Option<Rule>) {
        if let Some(rule) = rule {
            self.rules.add(rule, kind);
        }
    }

    fn require_rule(&self, index: usize, providers: &[Literal], reason: Reason) -> Option<Rule> {
        let mut literals = vec![-Pool::id(index)];
        for &provider in providers {
            if Pool::index(provider) == index {
                return None;
            }
            literals.push(provider);
        }
        Some(Rule::generic(literals, reason))
    }

    fn filtered(
        filter: &RequirementFilter,
        target: &str,
        constraint: &Constraint,
    ) -> Option<Constraint> {
        if filter.is_ignored(target) {
            return None;
        }
        Some(filter.filter_constraint(target, constraint))
    }

    fn add_rules_for_package(&mut self, start: usize, filter: &RequirementFilter) {
        let mut queue = VecDeque::from([start]);
        while let Some(index) = queue.pop_front() {
            if self.added.contains(&index) {
                continue;
            }
            self.added.insert(index);
            self.added_order.push(index);
            let package = &self.pool.packages[index];
            if let Some(real) = &package.alias_of {
                let real_index = self
                    .pool
                    .what_provides(&real.name, None)
                    .into_iter()
                    .map(Pool::index)
                    .find(|&i| {
                        let p = &self.pool.packages[i];
                        p.alias_of.is_none() && p.name == real.name && p.version == real.version
                    });
                if let Some(real_index) = real_index {
                    queue.push_back(real_index);
                    let rule = self.require_rule(
                        index,
                        &[Pool::id(real_index)],
                        Reason::PackageAlias(index),
                    );
                    self.add(TYPE_PACKAGE, rule);
                    let rule = self.require_rule(
                        real_index,
                        &[Pool::id(index)],
                        Reason::PackageInverseAlias(real_index),
                    );
                    self.add(TYPE_PACKAGE, rule);
                }
                if !package
                    .requires
                    .iter()
                    .any(|l| l.pretty_constraint == "self.version")
                {
                    continue;
                }
            } else {
                for name in super::names(package, false) {
                    match self.added_by_name.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, list)) => list.push(index),
                        None => self.added_by_name.push((name, vec![index])),
                    }
                }
            }
            for link in &package.requires {
                let Some(constraint) = Self::filtered(filter, &link.target, &link.constraint)
                else {
                    continue;
                };
                let providers = self.pool.what_provides(&link.target, Some(&constraint));
                let rule =
                    self.require_rule(index, &providers, Reason::PackageRequires(link.clone()));
                self.add(TYPE_PACKAGE, rule);
                queue.extend(providers.into_iter().map(Pool::index));
            }
        }
    }

    fn add_conflict_rules(&mut self, filter: &RequirementFilter) {
        for &index in &self.added_order.clone() {
            let package = &self.pool.packages[index];
            for link in &package.conflicts {
                if !self.added_by_name.iter().any(|(n, _)| *n == link.target) {
                    continue;
                }
                // conflicts never get their upper bound widened
                if filter.is_ignored(&link.target) {
                    continue;
                }
                for conflict in self
                    .pool
                    .what_provides(&link.target, Some(&link.constraint))
                {
                    let other = self.pool.package(conflict);
                    if other.alias_of.is_none() || other.name == link.target {
                        if Pool::index(conflict) == index {
                            continue;
                        }
                        self.rules.add(
                            Rule::two(
                                -Pool::id(index),
                                -conflict,
                                Reason::PackageConflict(link.clone()),
                            ),
                            TYPE_PACKAGE,
                        );
                    }
                }
            }
        }
        for (name, packages) in self.added_by_name.clone() {
            if packages.len() > 1 {
                let literals: Vec<Literal> = packages.iter().map(|&i| -Pool::id(i)).collect();
                let rule = if literals.len() == 2 {
                    Rule::two(literals[0], literals[1], Reason::PackageSameName(name))
                } else {
                    Rule::multi_conflict(literals, Reason::PackageSameName(name))
                };
                self.rules.add(rule, TYPE_PACKAGE);
            }
        }
    }

    fn add_rules_for_request(&mut self, request: &Request, filter: &RequirementFilter) {
        for (repo, package) in &request.fixed {
            let index = (0..self.pool.len()).find(|&i| {
                self.pool.repo[i] == *repo
                    && self.pool.packages[i].name == package.name
                    && self.pool.packages[i].version == package.version
            });
            let Some(index) = index else {
                // not in the pool because its stability was unacceptable
                continue;
            };
            self.add_rules_for_package(index, filter);
            self.rules.add(
                Rule::generic(vec![Pool::id(index)], Reason::Fixed(index)),
                TYPE_REQUEST,
            );
        }
        for (name, constraint, pretty) in &request.requires {
            let Some(filtered) = Self::filtered(filter, name, constraint) else {
                continue;
            };
            let pretty = if filtered == *constraint {
                pretty.clone()
            } else {
                filtered.to_string()
            };
            let packages = self.pool.what_provides(name, Some(&filtered));
            if !packages.is_empty() {
                for &literal in &packages {
                    self.add_rules_for_package(Pool::index(literal), filter);
                }
                self.rules.add(
                    Rule::generic(
                        packages,
                        Reason::RootRequire(name.clone(), filtered, pretty),
                    ),
                    TYPE_REQUEST,
                );
            }
        }
    }

    fn add_rules_for_root_aliases(&mut self, filter: &RequirementFilter) {
        for index in 0..self.pool.len() {
            let package = &self.pool.packages[index];
            if self.added.contains(&index) || package.alias_of.is_none() {
                continue;
            }
            let real = package.alias_of.as_ref().unwrap();
            let real_added = self.added.iter().any(|&i| {
                let p = &self.pool.packages[i];
                p.alias_of.is_none() && p.name == real.name && p.version == real.version
            });
            if package.root_alias || real_added {
                self.add_rules_for_package(index, filter);
            }
        }
    }
}

pub struct Solver<'a> {
    pub policy: &'a DefaultPolicy,
    pub pool: &'a Pool,
    rules: RuleSet,
    watch_graph: RuleWatchGraph,
    decisions: Decisions,
    fixed: HashSet<usize>,
    propagate_index: usize,
    branches: Vec<(Vec<Literal>, usize)>,
    problems: Vec<Problem>,
    pub learned_pool: Vec<Vec<usize>>,
    learned_why: HashMap<usize, usize>,
}

/// The solver's result: the decision queue (positive literals are the installed set).
pub struct Solution {
    pub decisions: Vec<(Literal, usize)>,
    pub rule_count: usize,
}

impl<'a> Solver<'a> {
    pub fn new(policy: &'a DefaultPolicy, pool: &'a Pool) -> Solver<'a> {
        Solver {
            policy,
            pool,
            rules: RuleSet::default(),
            watch_graph: RuleWatchGraph::default(),
            decisions: Decisions::default(),
            fixed: HashSet::new(),
            propagate_index: 0,
            branches: Vec::new(),
            problems: Vec::new(),
            learned_pool: Vec::new(),
            learned_why: HashMap::new(),
        }
    }

    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    pub fn solve(
        mut self,
        io: &mut Io,
        request: &Request,
        filter: &RequirementFilter,
    ) -> Result<(Solution, RuleSet), SolverProblems> {
        self.fixed = request.fixed_indices(self.pool);
        if io.verbosity >= 3 {
            io.error("Generating rules");
        }
        self.rules = generate_rules(self.pool, request, filter);
        self.check_for_root_require_problems(request, filter);
        for id in self.rules.all_ids() {
            self.watch_graph.insert(id, &self.rules, None);
        }
        self.make_assertion_rule_decisions();
        if io.verbosity >= 3 {
            io.error("Resolving dependencies through SAT");
        }
        self.run_sat();
        if !self.problems.is_empty() {
            return Err(SolverProblems {
                problems: self.problems,
                learned_pool: self.learned_pool,
            });
        }
        Ok((
            Solution {
                decisions: self.decisions.queue.clone(),
                rule_count: self.rules.len(),
            },
            self.rules,
        ))
    }

    fn check_for_root_require_problems(&mut self, request: &Request, filter: &RequirementFilter) {
        for (name, constraint, pretty) in &request.requires {
            if filter.is_ignored(name) {
                continue;
            }
            let filtered = filter.filter_constraint(name, constraint);
            let pretty = if filtered == *constraint {
                pretty.clone()
            } else {
                filtered.to_string()
            };
            if self.pool.what_provides(name, Some(&filtered)).is_empty() {
                let mut problem = Problem::default();
                problem.add_rule_detached(Rule::generic(
                    vec![],
                    Reason::RootRequire(name.clone(), filtered, pretty),
                ));
                self.problems.push(problem);
            }
        }
    }

    fn make_assertion_rule_decisions(&mut self) {
        let decision_start = self.decisions.len() as i64 - 1;
        let rules_count = self.rules.len();
        let mut rule_index: i64 = -1;
        loop {
            rule_index += 1;
            if rule_index as usize >= rules_count {
                break;
            }
            let id = rule_index as usize;
            let rule = &self.rules.rules[id];
            if !rule.is_assertion() || rule.disabled {
                continue;
            }
            let literal = rule.literals[0];
            if !self.decisions.decided(literal) {
                self.decisions.decide(literal, 1, id);
                continue;
            }
            if self.decisions.satisfy(literal) {
                continue;
            }
            if rule.kind == TYPE_LEARNED {
                self.rules.rules[id].disabled = true;
                continue;
            }
            let conflict = self.decisions.decision_rule(literal);
            if self.rules.rules[conflict].kind == TYPE_PACKAGE {
                let mut problem = Problem::default();
                problem.add_rule(id, &self.rules);
                problem.add_rule(conflict, &self.rules);
                self.rules.rules[id].disabled = true;
                self.problems.push(problem);
                continue;
            }
            let mut problem = Problem::default();
            problem.add_rule(id, &self.rules);
            problem.add_rule(conflict, &self.rules);
            for assert_id in self.rules.of_type(TYPE_REQUEST) {
                let assert_rule = &self.rules.rules[assert_id];
                if assert_rule.disabled || !assert_rule.is_assertion() {
                    continue;
                }
                if literal.abs() != assert_rule.literals[0].abs() {
                    continue;
                }
                problem.add_rule(assert_id, &self.rules);
                self.rules.rules[assert_id].disabled = true;
            }
            self.problems.push(problem);
            self.decisions.reset_to_offset(decision_start);
            rule_index = -1;
        }
    }

    fn propagate(&mut self, level: usize) -> Option<usize> {
        while self.propagate_index < self.decisions.len() {
            let literal = self.decisions.queue[self.propagate_index].0;
            let conflict = self.watch_graph.propagate_literal(
                literal,
                level,
                &mut self.decisions,
                &self.rules,
            );
            self.propagate_index += 1;
            if conflict.is_some() {
                return conflict;
            }
        }
        None
    }

    fn revert(&mut self, level: usize) {
        while !self.decisions.is_empty() {
            let literal = self.decisions.last_literal();
            if self.decisions.undecided(literal) {
                break;
            }
            if self.decisions.decision_level(literal) <= level {
                break;
            }
            self.decisions.revert_last();
            self.propagate_index = self.decisions.len();
        }
        while self.branches.last().is_some_and(|(_, l)| *l >= level) {
            self.branches.pop();
        }
    }

    fn set_propagate_learn(&mut self, mut level: usize, literal: Literal, rule: usize) -> usize {
        level += 1;
        self.decisions.decide(literal, level, rule);
        while let Some(conflict) = self.propagate(level) {
            if level == 1 {
                self.analyze_unsolvable(conflict);
                return 0;
            }
            let (learn_literal, new_level, new_rule, why) = self.analyze(level, conflict);
            assert!(
                new_level > 0 && new_level < level,
                "Trying to revert to invalid level {new_level} from level {level}."
            );
            level = new_level;
            self.revert(level);
            let id = self
                .rules
                .add(new_rule, TYPE_LEARNED)
                .expect("learned rule is new");
            self.learned_why.insert(id, why);
            self.watch_graph
                .insert(id, &self.rules, Some(&self.decisions));
            self.decisions.decide(learn_literal, level, id);
        }
        level
    }

    fn select_and_install(&mut self, level: usize, queue: Vec<Literal>, rule: usize) -> usize {
        let required = self.rules.rules[rule].required_package(self.pool);
        let mut literals =
            self.policy
                .select_preferred_packages(self.pool, queue, required.as_deref());
        let selected = literals.remove(0);
        if !literals.is_empty() {
            self.branches.push((literals, level));
        }
        self.set_propagate_learn(level, selected, rule)
    }

    fn analyze(&mut self, level: usize, mut rule: usize) -> (Literal, usize, Rule, usize) {
        let analyzed_rule = rule;
        let mut rule_level = 1;
        let mut num = 0;
        let mut l1num: i64 = 0;
        let mut seen: HashSet<usize> = HashSet::new();
        let mut learned_literal: Option<Literal> = None;
        let mut other_learned: Vec<Literal> = Vec::new();
        let mut decision_id = self.decisions.len();
        self.learned_pool.push(Vec::new());
        'outer: loop {
            self.learned_pool.last_mut().unwrap().push(rule);
            let (literals, multi) = {
                let r = &self.rules.rules[rule];
                (r.literals.clone(), r.multi_conflict)
            };
            for literal in literals {
                if multi && !self.decisions.decided(literal) {
                    continue;
                }
                if self.decisions.satisfy(literal) {
                    continue;
                }
                let index = Pool::index(literal);
                if seen.contains(&index) {
                    continue;
                }
                seen.insert(index);
                let l = self.decisions.decision_level(literal);
                if l == 1 {
                    l1num += 1;
                } else if l == level {
                    num += 1;
                } else {
                    other_learned.push(literal);
                    if l > rule_level {
                        rule_level = l;
                    }
                }
            }
            let mut l1retry = true;
            while l1retry {
                l1retry = false;
                if num == 0 {
                    l1num -= 1;
                    if l1num == 0 {
                        break 'outer;
                    }
                }
                let literal;
                loop {
                    assert!(
                        decision_id > 0,
                        "Reached invalid decision id {decision_id} while looking through {} for a literal present in the analyzed rule {}.",
                        self.rules.rules[rule],
                        self.rules.rules[analyzed_rule]
                    );
                    decision_id -= 1;
                    let l = self.decisions.queue[decision_id].0;
                    if seen.contains(&Pool::index(l)) {
                        literal = l;
                        break;
                    }
                }
                seen.remove(&Pool::index(literal));
                if num != 0 {
                    num -= 1;
                    if num == 0 {
                        learned_literal = Some(-literal);
                        if l1num == 0 {
                            break 'outer;
                        }
                        for other in &other_learned {
                            seen.remove(&Pool::index(*other));
                        }
                        l1num += 1;
                        l1retry = true;
                        continue;
                    }
                }
                let reason = self.decisions.queue[decision_id].1;
                rule = reason;
                if self.rules.rules[rule].multi_conflict {
                    let rule_literals = self.rules.rules[rule].literals.clone();
                    for rule_literal in rule_literals {
                        if !seen.contains(&Pool::index(rule_literal))
                            && self.decisions.satisfy(-rule_literal)
                        {
                            self.learned_pool.last_mut().unwrap().push(rule);
                            let l = self.decisions.decision_level(rule_literal);
                            if l == 1 {
                                l1num += 1;
                            } else if l == level {
                                num += 1;
                            } else {
                                other_learned.push(rule_literal);
                                if l > rule_level {
                                    rule_level = l;
                                }
                            }
                            seen.insert(Pool::index(rule_literal));
                            break;
                        }
                    }
                    l1retry = true;
                }
            }
            rule = self.decisions.queue[decision_id].1;
        }
        let why = self.learned_pool.len() - 1;
        let learned_literal = learned_literal.unwrap_or_else(|| {
            panic!(
                "Did not find a learnable literal in analyzed rule {}.",
                self.rules.rules[analyzed_rule]
            )
        });
        let mut literals = vec![learned_literal];
        literals.extend(other_learned);
        let new_rule = Rule::generic(literals, Reason::Learned(why));
        (learned_literal, rule_level, new_rule, why)
    }

    fn analyze_unsolvable_rule(
        &self,
        problem: &mut Problem,
        conflict: usize,
        seen: &mut HashSet<usize>,
    ) {
        seen.insert(conflict);
        let rule = &self.rules.rules[conflict];
        if rule.kind == TYPE_LEARNED {
            let why = self.learned_why[&conflict];
            for &problem_rule in &self.learned_pool[why] {
                if !seen.contains(&problem_rule) {
                    self.analyze_unsolvable_rule(problem, problem_rule, seen);
                }
            }
            return;
        }
        if rule.kind == TYPE_PACKAGE {
            return;
        }
        problem.next_section();
        problem.add_rule(conflict, &self.rules);
    }

    fn analyze_unsolvable(&mut self, conflict: usize) {
        let mut problem = Problem::default();
        problem.add_rule(conflict, &self.rules);
        let mut rule_seen = HashSet::new();
        self.analyze_unsolvable_rule(&mut problem, conflict, &mut rule_seen);
        let mut seen: HashSet<usize> = HashSet::new();
        for &literal in &self.rules.rules[conflict].literals {
            if self.decisions.satisfy(literal) {
                continue;
            }
            seen.insert(Pool::index(literal));
        }
        for i in (0..self.decisions.len()).rev() {
            let (decision_literal, why) = self.decisions.queue[i];
            if !seen.contains(&Pool::index(decision_literal)) {
                continue;
            }
            problem.add_rule(why, &self.rules);
            self.analyze_unsolvable_rule(&mut problem, why, &mut rule_seen);
            for &literal in &self.rules.rules[why].literals {
                if self.decisions.satisfy(literal) {
                    continue;
                }
                seen.insert(Pool::index(literal));
            }
        }
        self.problems.push(problem);
    }

    fn run_sat(&mut self) {
        self.propagate_index = 0;
        let mut level = 1;
        let mut system_level = level + 1;
        loop {
            if level == 1
                && let Some(conflict) = self.propagate(level)
            {
                self.analyze_unsolvable(conflict);
                return;
            }
            if level < system_level {
                let request_rules = self.rules.of_type(TYPE_REQUEST);
                let mut position = 0;
                let mut broke = false;
                for (pos, id) in request_rules.iter().enumerate() {
                    position = pos;
                    let rule = &self.rules.rules[*id];
                    if rule.disabled {
                        continue;
                    }
                    let mut queue = Vec::new();
                    let mut none_satisfied = true;
                    for &literal in &rule.literals {
                        if self.decisions.satisfy(literal) {
                            none_satisfied = false;
                            break;
                        }
                        if literal > 0 && self.decisions.undecided(literal) {
                            queue.push(literal);
                        }
                    }
                    if none_satisfied && !queue.is_empty() {
                        let pruned: Vec<Literal> = queue
                            .iter()
                            .copied()
                            .filter(|l| self.fixed.contains(&Pool::index(*l)))
                            .collect();
                        if !pruned.is_empty() {
                            queue = pruned;
                        }
                    }
                    if none_satisfied && !queue.is_empty() {
                        let old_level = level;
                        level = self.select_and_install(level, queue, *id);
                        if level == 0 {
                            return;
                        }
                        if level <= old_level {
                            broke = true;
                            break;
                        }
                    }
                }
                system_level = level + 1;
                // `$iterator->next(); if ($iterator->valid()) continue;`
                if broke && position + 1 < request_rules.len() {
                    continue;
                }
            }
            if level < system_level {
                system_level = level;
            }
            let mut rules_count = self.rules.len();
            let mut i = 0;
            let mut n = 0;
            while n < rules_count {
                if i == rules_count {
                    i = 0;
                }
                let rule = &self.rules.rules[i];
                if !rule.disabled {
                    let mut queue = Vec::new();
                    let mut skip = false;
                    for &literal in &rule.literals {
                        if literal <= 0 {
                            if !self.decisions.decided_install(literal) {
                                skip = true;
                                break;
                            }
                        } else {
                            if self.decisions.decided_install(literal) {
                                skip = true;
                                break;
                            }
                            if self.decisions.undecided(literal) {
                                queue.push(literal);
                            }
                        }
                    }
                    if !skip && queue.len() >= 2 {
                        level = self.select_and_install(level, queue, i);
                        if level == 0 {
                            return;
                        }
                        rules_count = self.rules.len();
                        n = 0;
                        i += 1;
                        continue;
                    }
                }
                i += 1;
                n += 1;
            }
            if level < system_level {
                continue;
            }
            if !self.branches.is_empty() {
                let mut last: Option<(Literal, usize, usize, usize)> = None;
                for bi in (0..self.branches.len()).rev() {
                    let (literals, l) = &self.branches[bi];
                    for (offset, &literal) in literals.iter().enumerate() {
                        if literal > 0 && self.decisions.decision_level(literal) > l + 1 {
                            last = Some((literal, bi, offset, *l));
                        }
                    }
                }
                if let Some((literal, bi, offset, l)) = last {
                    self.branches[bi].0.remove(offset);
                    level = l;
                    self.revert(level);
                    let why = self.decisions.last_reason();
                    level = self.set_propagate_learn(level, literal, why);
                    if level == 0 {
                        return;
                    }
                    continue;
                }
            }
            break;
        }
    }
}
