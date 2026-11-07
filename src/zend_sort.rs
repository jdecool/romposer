//! Port of PHP's `zend_sort` (hybrid insertion sort / quicksort) so that `usort`
//! with an inconsistent comparator (composer/semver's branch-aware ordering)
//! yields Composer's exact order. PHP 8 makes `usort` stable by falling back to
//! the original position when the comparator returns 0; `usort` does the same.

use std::cmp::Ordering;

/// PHP `usort`: `cmp` is the user callback, ties keep the original order.
pub fn usort<T: Clone>(items: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let mut order: Vec<usize> = (0..items.len()).collect();
    zend_sort(&mut order, &mut |a, b| match cmp(&items[*a], &items[*b]) {
        Ordering::Equal => a.cmp(b),
        other => other,
    });
    let sorted: Vec<T> = order.iter().map(|&i| items[i].clone()).collect();
    items.clone_from_slice(&sorted);
}

fn gt<T>(v: &[T], cmp: &mut dyn FnMut(&T, &T) -> Ordering, a: usize, b: usize) -> bool {
    cmp(&v[a], &v[b]) == Ordering::Greater
}

fn sort2<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering, a: usize, b: usize) {
    if gt(v, cmp, a, b) {
        v.swap(a, b);
    }
}

fn sort3<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering, a: usize, b: usize, c: usize) {
    if !gt(v, cmp, a, b) {
        if !gt(v, cmp, b, c) {
            return;
        }
        v.swap(b, c);
        if gt(v, cmp, a, b) {
            v.swap(a, b);
        }
        return;
    }
    if !gt(v, cmp, c, b) {
        v.swap(a, c);
        return;
    }
    v.swap(a, b);
    if gt(v, cmp, b, c) {
        v.swap(b, c);
    }
}

fn sort4<T>(
    v: &mut [T],
    cmp: &mut dyn FnMut(&T, &T) -> Ordering,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) {
    sort3(v, cmp, a, b, c);
    if gt(v, cmp, c, d) {
        v.swap(c, d);
        if gt(v, cmp, b, c) {
            v.swap(b, c);
            if gt(v, cmp, a, b) {
                v.swap(a, b);
            }
        }
    }
}

fn sort5<T>(
    v: &mut [T],
    cmp: &mut dyn FnMut(&T, &T) -> Ordering,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    e: usize,
) {
    sort4(v, cmp, a, b, c, d);
    if gt(v, cmp, d, e) {
        v.swap(d, e);
        if gt(v, cmp, c, d) {
            v.swap(c, d);
            if gt(v, cmp, b, c) {
                v.swap(b, c);
                if gt(v, cmp, a, b) {
                    v.swap(a, b);
                }
            }
        }
    }
}

fn insert_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let n = v.len();
    match n {
        0 | 1 => {}
        2 => sort2(v, cmp, 0, 1),
        3 => sort3(v, cmp, 0, 1, 2),
        4 => sort4(v, cmp, 0, 1, 2, 3),
        5 => sort5(v, cmp, 0, 1, 2, 3, 4),
        _ => {
            let sentry = 6;
            for i in 1..sentry {
                let mut j = i - 1;
                if !gt(v, cmp, j, i) {
                    continue;
                }
                while j != 0 {
                    j -= 1;
                    if !gt(v, cmp, j, i) {
                        j += 1;
                        break;
                    }
                }
                let mut k = i;
                while k > j {
                    v.swap(k, k - 1);
                    k -= 1;
                }
            }
            for i in sentry..n {
                let mut j = i - 1;
                if !gt(v, cmp, j, i) {
                    continue;
                }
                loop {
                    j -= 2;
                    if !gt(v, cmp, j, i) {
                        j += 1;
                        if !gt(v, cmp, j, i) {
                            j += 1;
                        }
                        break;
                    }
                    if j == 0 {
                        break;
                    }
                    if j == 1 {
                        j -= 1;
                        if gt(v, cmp, i, j) {
                            j += 1;
                        }
                        break;
                    }
                }
                let mut k = i;
                while k > j {
                    v.swap(k, k - 1);
                    k -= 1;
                }
            }
        }
    }
}

/// `zend_sort` on `v[start..end]`.
pub fn zend_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let (mut start, mut end) = (0usize, v.len());
    loop {
        let nmemb = end - start;
        if nmemb <= 16 {
            insert_sort(&mut v[start..end], cmp);
            return;
        }
        let offset = nmemb >> 1;
        let pivot = start + offset;
        if (nmemb >> 10) != 0 {
            let delta = offset >> 1;
            sort5(v, cmp, start, start + delta, pivot, pivot + delta, end - 1);
        } else {
            sort3(v, cmp, start, pivot, end - 1);
        }
        v.swap(start + 1, pivot);
        let pivot = start + 1;
        let mut i = pivot + 1;
        let mut j = end - 1;
        'outer: loop {
            while gt(v, cmp, pivot, i) {
                i += 1;
                if i == j {
                    break 'outer;
                }
            }
            j -= 1;
            if j == i {
                break;
            }
            while gt(v, cmp, j, pivot) {
                j -= 1;
                if j == i {
                    break 'outer;
                }
            }
            v.swap(i, j);
            i += 1;
            if i == j {
                break;
            }
        }
        v.swap(pivot, i - 1);
        if (i - 1) - start < end - i {
            zend_sort(&mut v[start..i - 1], cmp);
            start = i;
        } else {
            zend_sort(&mut v[i..end], cmp);
            end = i - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_consistent_comparators_like_any_sort() {
        for n in [0usize, 1, 2, 3, 4, 5, 6, 9, 16, 17, 40, 1100] {
            let mut v: Vec<u32> = (0..n as u32).map(|i| (i * 7919) % 101).collect();
            let mut expected = v.clone();
            expected.sort();
            usort(&mut v, |a, b| a.cmp(b));
            assert_eq!(v, expected, "n={n}");
        }
    }

    #[test]
    fn stable_fallback_keeps_insertion_order_on_ties() {
        let mut v = vec![
            (1, 'a'),
            (0, 'b'),
            (1, 'c'),
            (0, 'd'),
            (1, 'e'),
            (0, 'f'),
            (1, 'g'),
        ];
        usort(&mut v, |a, b| a.0.cmp(&b.0));
        let order: String = v.iter().map(|(_, c)| *c).collect();
        assert_eq!(order, "bdfaceg");
    }
}
