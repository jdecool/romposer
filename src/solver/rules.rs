//! Rules, the rule set, the watch graph and the decision stack
//! (`Rule`, `GenericRule`, `Rule2Literals`, `MultiConflictRule`, `RuleSet`,
//! `RuleWatchGraph`, `RuleWatchNode`, `Decisions`).

use super::{Literal, Pool};
use crate::package::Link;
use crate::semver::Constraint;
use std::collections::{HashMap, HashSet, VecDeque};

pub const TYPE_PACKAGE: u8 = 0;
pub const TYPE_REQUEST: u8 = 1;
pub const TYPE_LEARNED: u8 = 4;
pub const TYPES: [u8; 3] = [TYPE_PACKAGE, TYPE_REQUEST, TYPE_LEARNED];

#[derive(Clone, Debug)]
pub enum Reason {
    /// name, constraint, pretty constraint
    RootRequire(String, Constraint, String),
    /// pool index of the fixed package
    Fixed(usize),
    PackageConflict(Link),
    PackageRequires(Link),
    PackageSameName(String),
    /// index into the solver's learned pool
    Learned(usize),
    /// pool index of the alias package
    PackageAlias(usize),
    /// pool index of the aliased package
    PackageInverseAlias(usize),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub literals: Vec<Literal>,
    pub multi_conflict: bool,
    pub reason: Reason,
    pub kind: u8,
    pub disabled: bool,
}

impl Rule {
    pub fn generic(mut literals: Vec<Literal>, reason: Reason) -> Rule {
        literals.sort_unstable();
        Rule {
            literals,
            multi_conflict: false,
            reason,
            kind: 255,
            disabled: false,
        }
    }

    pub fn two(a: Literal, b: Literal, reason: Reason) -> Rule {
        Rule::generic(vec![a, b], reason)
    }

    pub fn multi_conflict(mut literals: Vec<Literal>, reason: Reason) -> Rule {
        literals.sort_unstable();
        Rule {
            literals,
            multi_conflict: true,
            reason,
            kind: 255,
            disabled: false,
        }
    }

    pub fn is_assertion(&self) -> bool {
        !self.multi_conflict && self.literals.len() == 1
    }

    pub fn required_package(&self, pool: &Pool) -> Option<String> {
        match &self.reason {
            Reason::RootRequire(name, _, _) => Some(name.clone()),
            Reason::Fixed(i) => Some(pool.packages[*i].name.clone()),
            Reason::PackageRequires(link) => Some(link.target.clone()),
            _ => None,
        }
    }

    pub fn reason_code(&self) -> u8 {
        match self.reason {
            Reason::RootRequire(..) => 2,
            Reason::Fixed(_) => 3,
            Reason::PackageConflict(_) => 6,
            Reason::PackageRequires(_) => 7,
            Reason::PackageSameName(_) => 10,
            Reason::Learned(_) => 12,
            Reason::PackageAlias(_) => 13,
            Reason::PackageInverseAlias(_) => 14,
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let literals: Vec<String> = self.literals.iter().map(|l| l.to_string()).collect();
        let open = if self.disabled { "disabled(" } else { "(" };
        if self.multi_conflict {
            write!(f, "{open}multi({}))", literals.join("|"))
        } else {
            write!(f, "{open}{})", literals.join("|"))
        }
    }
}

#[derive(Default)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub by_type: HashMap<u8, Vec<usize>>,
    seen: HashSet<(bool, Vec<Literal>)>,
}

impl RuleSet {
    /// Adds the rule unless an equal one exists; returns its id when added.
    pub fn add(&mut self, mut rule: Rule, kind: u8) -> Option<usize> {
        let key = (rule.multi_conflict, rule.literals.clone());
        if self.seen.contains(&key) {
            return None;
        }
        self.seen.insert(key);
        rule.kind = kind;
        let id = self.rules.len();
        self.rules.push(rule);
        self.by_type.entry(kind).or_default().push(id);
        Some(id)
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn of_type(&self, kind: u8) -> Vec<usize> {
        self.by_type.get(&kind).cloned().unwrap_or_default()
    }

    /// Every rule id in `RuleSetIterator` order: by type, then insertion.
    pub fn all_ids(&self) -> Vec<usize> {
        TYPES.iter().flat_map(|t| self.of_type(*t)).collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct WatchNode {
    rule: usize,
    watch1: Literal,
    watch2: Literal,
}

impl WatchNode {
    fn new(rule_id: usize, rule: &Rule) -> WatchNode {
        WatchNode {
            rule: rule_id,
            watch1: rule.literals.first().copied().unwrap_or(0),
            watch2: rule.literals.get(1).copied().unwrap_or(0),
        }
    }

    fn other_watch(&self, literal: Literal) -> Literal {
        if self.watch1 == literal {
            self.watch2
        } else {
            self.watch1
        }
    }
}

#[derive(Default)]
pub struct RuleWatchGraph {
    nodes: Vec<WatchNode>,
    chains: HashMap<Literal, VecDeque<usize>>,
}

impl RuleWatchGraph {
    pub fn insert(&mut self, rule_id: usize, rules: &RuleSet, decisions: Option<&Decisions>) {
        let rule = &rules.rules[rule_id];
        if rule.is_assertion() {
            return;
        }
        let mut node = WatchNode::new(rule_id, rule);
        // `watch2OnHighest` for learned rules
        if let Some(decisions) = decisions
            && rule.literals.len() >= 3
            && !rule.multi_conflict
        {
            let mut watch_level = 0;
            for &literal in &rule.literals {
                let level = decisions.decision_level(literal);
                if level > watch_level {
                    node.watch2 = literal;
                    watch_level = level;
                }
            }
        }
        let index = self.nodes.len();
        self.nodes.push(node);
        let literals: Vec<Literal> = if rule.multi_conflict {
            rule.literals.clone()
        } else {
            vec![node.watch1, node.watch2]
        };
        for literal in literals {
            self.chains.entry(literal).or_default().push_front(index);
        }
    }

    /// `propagateLiteral`: returns the conflicting rule, if any.
    pub fn propagate_literal(
        &mut self,
        decided: Literal,
        level: usize,
        decisions: &mut Decisions,
        rules: &RuleSet,
    ) -> Option<usize> {
        let literal = -decided;
        let mut i = 0;
        loop {
            let &node_index = self.chains.get(&literal).and_then(|c| c.get(i))?;
            let node = self.nodes[node_index];
            let rule = &rules.rules[node.rule];
            if !rule.multi_conflict {
                let other_watch = node.other_watch(literal);
                if !rule.disabled && !decisions.satisfy(other_watch) {
                    let alternative = rule
                        .literals
                        .iter()
                        .copied()
                        .find(|&l| l != literal && l != other_watch && !decisions.conflict(l));
                    if let Some(alternative) = alternative {
                        // moveWatch: re-point the node and move it to the other chain
                        let node = &mut self.nodes[node_index];
                        if node.watch1 == literal {
                            node.watch1 = alternative;
                        } else {
                            node.watch2 = alternative;
                        }
                        self.chains.get_mut(&literal).unwrap().remove(i);
                        self.chains
                            .entry(alternative)
                            .or_default()
                            .push_front(node_index);
                        continue;
                    }
                    if decisions.conflict(other_watch) {
                        return Some(node.rule);
                    }
                    decisions.decide(other_watch, level, node.rule);
                }
            } else {
                for &other in &rule.literals {
                    if literal != other && !decisions.satisfy(other) {
                        if decisions.conflict(other) {
                            return Some(node.rule);
                        }
                        decisions.decide(other, level, node.rule);
                    }
                }
            }
            i += 1;
        }
    }
}

#[derive(Default)]
pub struct Decisions {
    /// package index => ±level
    map: HashMap<usize, i64>,
    /// (literal, rule id)
    pub queue: Vec<(Literal, usize)>,
}

impl Decisions {
    pub fn decide(&mut self, literal: Literal, level: usize, why: usize) {
        let index = Pool::index(literal);
        let previous = self.map.get(&index).copied().unwrap_or(0);
        assert!(
            previous == 0,
            "Trying to decide {literal} on level {level}, even though it was previously decided as {previous}."
        );
        self.map.insert(
            index,
            if literal > 0 {
                level as i64
            } else {
                -(level as i64)
            },
        );
        self.queue.push((literal, why));
    }

    pub fn satisfy(&self, literal: Literal) -> bool {
        let d = self.map.get(&Pool::index(literal)).copied().unwrap_or(0);
        (literal > 0 && d > 0) || (literal < 0 && d < 0)
    }

    pub fn conflict(&self, literal: Literal) -> bool {
        let d = self.map.get(&Pool::index(literal)).copied().unwrap_or(0);
        (d > 0 && literal < 0) || (d < 0 && literal > 0)
    }

    pub fn decided(&self, literal: Literal) -> bool {
        self.map.get(&Pool::index(literal)).copied().unwrap_or(0) != 0
    }

    pub fn undecided(&self, literal: Literal) -> bool {
        !self.decided(literal)
    }

    pub fn decided_install(&self, literal: Literal) -> bool {
        self.map.get(&Pool::index(literal)).copied().unwrap_or(0) > 0
    }

    pub fn decision_level(&self, literal: Literal) -> usize {
        self.map
            .get(&Pool::index(literal))
            .copied()
            .unwrap_or(0)
            .unsigned_abs() as usize
    }

    pub fn decision_rule(&self, literal: Literal) -> usize {
        let index = Pool::index(literal);
        self.queue
            .iter()
            .find(|(l, _)| Pool::index(*l) == index)
            .map(|(_, r)| *r)
            .expect("decision rule")
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn last_literal(&self) -> Literal {
        self.queue.last().unwrap().0
    }

    pub fn last_reason(&self) -> usize {
        self.queue.last().unwrap().1
    }

    pub fn reset_to_offset(&mut self, offset: i64) {
        while self.queue.len() as i64 > offset + 1 {
            let (literal, _) = self.queue.pop().unwrap();
            self.map.insert(Pool::index(literal), 0);
        }
    }

    pub fn revert_last(&mut self) {
        let (literal, _) = self.queue.pop().unwrap();
        self.map.insert(Pool::index(literal), 0);
    }
}
