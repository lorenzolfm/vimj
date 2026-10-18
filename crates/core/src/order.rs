//! Song order operations: moving and smart reordering.

/// Moves `id` to sit directly after `anchor`. Returns false if either is
/// missing or they are the same song.
pub fn place_after(order: &mut Vec<String>, anchor: &str, id: &str) -> bool {
    if anchor == id || !order.iter().any(|x| x == anchor) {
        return false;
    }
    let Some(from) = order.iter().position(|x| x == id) else {
        return false;
    };
    let moved = order.remove(from);
    let at = order.iter().position(|x| x == anchor).expect("anchor is present") + 1;
    order.insert(at, moved);
    true
}

/// Moves the item at `from` to index `to` (clamped).
pub fn move_to(order: &mut Vec<String>, from: usize, to: usize) -> bool {
    move_block(order, from, 1, to)
}

/// Moves the `count` items starting at `from` so the block starts at index
/// `to` afterwards (clamped). Returns whether anything moved.
pub fn move_block(order: &mut Vec<String>, from: usize, count: usize, to: usize) -> bool {
    if count == 0 || from >= order.len() {
        return false;
    }
    let count = count.min(order.len() - from);
    let block: Vec<String> = order.drain(from..from + count).collect();
    let to = to.min(order.len());
    order.splice(to..to, block);
    from != to
}

/// Number of starting points tried by the greedy pass on long lists.
const MAX_STARTS: usize = 64;
/// Upper bound on improvement passes.
const MAX_PASSES: usize = 30;

/// Orders `n` items so that the summed `cost(a, b)` of consecutive pairs is
/// low. `cost` may be asymmetric. Returns a permutation of `0..n`.
///
/// Greedy nearest-neighbour from several starts, then relocate moves until no
/// single move improves the path.
pub fn smart_order(n: usize, cost: impl Fn(usize, usize) -> f32) -> Vec<usize> {
    if n <= 2 {
        return (0..n).collect();
    }
    let matrix: Vec<f32> = (0..n * n).map(|i| cost(i / n, i % n)).collect();
    let c = |a: usize, b: usize| matrix[a * n + b];

    let starts: Vec<usize> = if n <= MAX_STARTS {
        (0..n).collect()
    } else {
        (0..MAX_STARTS).map(|i| i * n / MAX_STARTS).collect()
    };
    let mut best = starts
        .into_iter()
        .map(|start| greedy_from(n, start, &c))
        .min_by(|a, b| path_cost(a, &c).total_cmp(&path_cost(b, &c)))
        .expect("at least one start");

    for _ in 0..MAX_PASSES {
        if !relocate_pass(&mut best, &c) {
            break;
        }
    }
    best
}

fn greedy_from(n: usize, start: usize, c: &impl Fn(usize, usize) -> f32) -> Vec<usize> {
    let mut used = vec![false; n];
    let mut path = Vec::with_capacity(n);
    let mut at = start;
    used[at] = true;
    path.push(at);
    while path.len() < n {
        let next = (0..n)
            .filter(|&j| !used[j])
            .min_by(|&x, &y| c(at, x).total_cmp(&c(at, y)))
            .expect("unused item remains");
        used[next] = true;
        path.push(next);
        at = next;
    }
    path
}

pub fn path_cost(path: &[usize], c: &impl Fn(usize, usize) -> f32) -> f32 {
    path.windows(2).map(|w| c(w[0], w[1])).sum()
}

/// Tries moving each item to every other slot; applies the first improving
/// move found per item. Returns whether anything changed.
fn relocate_pass(path: &mut Vec<usize>, c: &impl Fn(usize, usize) -> f32) -> bool {
    const EPSILON: f32 = 1e-6;
    let mut improved = false;
    for i in 0..path.len() {
        let k = path[i];
        let prev = i.checked_sub(1).map(|p| path[p]);
        let next = path.get(i + 1).copied();
        let edge = |a: Option<usize>, b: Option<usize>| match (a, b) {
            (Some(a), Some(b)) => c(a, b),
            _ => 0.0,
        };
        let removal_gain = edge(prev, Some(k)) + edge(Some(k), next) - edge(prev, next);

        let mut rest = path.clone();
        rest.remove(i);
        let best_slot = (0..=rest.len())
            .filter(|&j| j != i)
            .map(|j| {
                let u = j.checked_sub(1).map(|p| rest[p]);
                let v = rest.get(j).copied();
                (j, edge(u, Some(k)) + edge(Some(k), v) - edge(u, v))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((j, insertion_cost)) = best_slot
            && insertion_cost < removal_gain - EPSILON
        {
            rest.insert(j, k);
            *path = rest;
            improved = true;
        }
    }
    improved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn place_after_moves_forward_and_backward() {
        let mut order = ids(&["a", "b", "c", "d"]);
        assert!(place_after(&mut order, "a", "d"));
        assert_eq!(order, ids(&["a", "d", "b", "c"]));
        assert!(place_after(&mut order, "c", "a"));
        assert_eq!(order, ids(&["d", "b", "c", "a"]));
        assert!(!place_after(&mut order, "a", "a"));
        assert!(!place_after(&mut order, "zz", "a"));
    }

    #[test]
    fn move_block_moves_runs_both_ways() {
        let mut order = ids(&["a", "b", "c", "d", "e"]);
        assert!(move_block(&mut order, 1, 2, 3));
        assert_eq!(order, ids(&["a", "d", "e", "b", "c"]));
        assert!(move_block(&mut order, 3, 2, 0));
        assert_eq!(order, ids(&["b", "c", "a", "d", "e"]));
        assert!(!move_block(&mut order, 0, 2, 0));
    }

    #[test]
    fn move_to_clamps() {
        let mut order = ids(&["a", "b", "c"]);
        assert!(move_to(&mut order, 0, 99));
        assert_eq!(order, ids(&["b", "c", "a"]));
        assert!(!move_to(&mut order, 9, 0));
    }

    #[test]
    fn recovers_a_chain_from_shuffled_input() {
        // Items lie on a line; consecutive values are cheap to join.
        let values = [5.0_f32, 1.0, 9.0, 3.0, 7.0, 2.0, 8.0, 4.0, 6.0, 0.0];
        let path = smart_order(values.len(), |a, b| (values[a] - values[b]).abs());
        let walked: Vec<f32> = path.iter().map(|&i| values[i]).collect();
        let total: f32 = walked.windows(2).map(|w| (w[0] - w[1]).abs()).sum();
        assert_eq!(total, 9.0, "{walked:?}");
    }

    #[test]
    fn returns_a_permutation() {
        let n = 150;
        let path = smart_order(n, |a, b| ((a * 7 + b * 13) % 17) as f32);
        let mut sorted = path.clone();
        sorted.sort();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn relocation_never_makes_the_path_worse() {
        let n = 40;
        let c = |a: usize, b: usize| ((a * 31 + b * 17) % 23) as f32 + if b == a + 1 { -5.0 } else { 0.0 };
        let greedy = greedy_from(n, 0, &c);
        let mut improved = greedy.clone();
        while relocate_pass(&mut improved, &c) {}
        assert!(path_cost(&improved, &c) <= path_cost(&greedy, &c));
    }
}
