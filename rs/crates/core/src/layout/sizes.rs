//! Port of `src/renderer/layout/sizes.ts` — split-fraction normalization + resize helpers:
//! `clampFraction` / `equalSizes` / `removeSize` / `insertSize` (sizes always re-normalize to
//! sum = 1). Mirror `sizes.test.ts`.
//!
//! Fractional-size helpers shared by the layout engine and the resize dividers.
//! The insert/remove rebalancing is ported from vercel/hyper's term-groups reducer.

pub const MIN_SIZE: f64 = 0.05;

/// `n` equal fractions summing to 1 (empty for `n == 0`).
pub fn equal_sizes(n: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    vec![1.0 / n as f64; n]
}

/// Scale any positive vector to sum 1; a non-positive sum falls back to an equal split.
pub fn normalize(sizes: &[f64]) -> Vec<f64> {
    let sum: f64 = sizes.iter().sum();
    if sum <= 0.0 {
        return equal_sizes(sizes.len());
    }
    sizes.iter().map(|s| s / sum).collect()
}

/// Are these sizes an (≈) equal split? Used to skip serializing default splits.
pub fn is_equal_split(sizes: &[f64]) -> bool {
    if sizes.len() <= 1 {
        return true;
    }
    let eq = 1.0 / sizes.len() as f64;
    sizes.iter().all(|s| (s - eq).abs() < 1e-6)
}

/// Bound a fraction within `[MIN_SIZE, 1 - MIN_SIZE]`.
pub fn clamp_fraction(f: f64) -> f64 {
    f.clamp(MIN_SIZE, 1.0 - MIN_SIZE)
}

/// Insert a slot at `index`, shrinking the others proportionally (Hyper insertRebalance).
pub fn insert_size(sizes: &[f64], index: usize) -> Vec<f64> {
    if sizes.is_empty() {
        return vec![1.0];
    }
    let new_size = 1.0 / (sizes.len() + 1) as f64;
    let balanced: Vec<f64> = sizes.iter().map(|s| s - new_size * s).collect();
    let mut out = Vec::with_capacity(balanced.len() + 1);
    out.extend_from_slice(&balanced[..index]);
    out.push(new_size);
    out.extend_from_slice(&balanced[index..]);
    out
}

/// Remove the slot at `index`, spreading its size across the rest (Hyper removalRebalance).
pub fn remove_size(sizes: &[f64], index: usize) -> Vec<f64> {
    if sizes.len() <= 1 {
        return Vec::new();
    }
    let removed = sizes[index];
    let increase = removed / (sizes.len() - 1) as f64;
    sizes
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != index)
        .map(|(_, s)| s + increase)
        .collect()
}

/// Move the boundary between slot `i` and `i+1` by `delta` (a fraction), clamped.
pub fn resize_at(sizes: &[f64], i: usize, delta: f64) -> Vec<f64> {
    if i + 1 >= sizes.len() {
        return sizes.to_vec();
    }
    let a = sizes[i] + delta;
    let b = sizes[i + 1] - delta;
    if a < MIN_SIZE || b < MIN_SIZE {
        return sizes.to_vec();
    }
    let mut next = sizes.to_vec();
    next[i] = a;
    next[i + 1] = b;
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(a: &[f64]) -> f64 {
        a.iter().sum()
    }

    // vitest `toBeCloseTo` defaults to 2 decimal digits → within 0.005.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.005
    }

    #[test]
    fn equal_sizes_sums_to_1_and_handles_empty() {
        assert!(close(sum(&equal_sizes(4)), 1.0));
        assert_eq!(equal_sizes(0), Vec::<f64>::new());
    }

    #[test]
    fn insert_size_adds_a_slot_keeping_the_total_1() {
        let s = insert_size(&equal_sizes(2), 1);
        assert_eq!(s.len(), 3);
        assert!(close(sum(&s), 1.0));
    }

    #[test]
    fn remove_size_drops_a_slot_keeping_the_total_1() {
        let s = remove_size(&equal_sizes(3), 0);
        assert_eq!(s.len(), 2);
        assert!(close(sum(&s), 1.0));
    }

    #[test]
    fn resize_at_shifts_the_boundary_and_respects_min_size() {
        let s = resize_at(&[0.5, 0.5], 0, 0.1);
        assert!(close(s[0], 0.6));
        assert!(close(s[1], 0.4));
        // clamped: cannot push a neighbour below MIN_SIZE
        assert_eq!(resize_at(&[0.5, 0.5], 0, -0.9), vec![0.5, 0.5]);
    }

    #[test]
    fn clamp_fraction_bounds_within_range() {
        assert_eq!(clamp_fraction(0.0), MIN_SIZE);
        assert_eq!(clamp_fraction(1.0), 1.0 - MIN_SIZE);
        assert_eq!(clamp_fraction(0.5), 0.5);
    }

    #[test]
    fn normalize_scales_any_positive_vector_to_sum_1() {
        assert!(close(sum(&normalize(&[1.0, 1.0, 2.0])), 1.0));
    }
}
