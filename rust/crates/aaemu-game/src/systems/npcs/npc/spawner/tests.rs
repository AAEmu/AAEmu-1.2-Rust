use super::*;

#[test]
fn time_between_inclusive_and_wrapping() {
    // Normal window.
    assert!(is_hour_between(10.0, 8.0, 20.0));
    assert!(is_hour_between(8.0, 8.0, 20.0));
    assert!(is_hour_between(20.0, 8.0, 20.0));
    assert!(!is_hour_between(7.99, 8.0, 20.0));
    assert!(!is_hour_between(20.01, 8.0, 20.0));
    // Wrapping over midnight.
    assert!(is_hour_between(23.0, 22.0, 4.0));
    assert!(is_hour_between(1.0, 22.0, 4.0));
    assert!(is_hour_between(4.0, 22.0, 4.0));
    assert!(!is_hour_between(12.0, 22.0, 4.0));
    // Equal start / end: only that instant.
    assert!(is_hour_between(5.0, 5.0, 5.0));
    assert!(!is_hour_between(5.5, 5.0, 5.0));
    assert_eq!(ticks_from_hours(1.0), 36_000_000_000);
    assert_eq!(ticks_from_hours(0.5), 18_000_000_000);
}

#[test]
fn random_helpers() {
    for _ in 0..200 {
        let v = random_float(10.0, 20.0);
        assert!((10.0..20.0).contains(&v));
        assert_eq!(random_float(7.0, 7.0), 7.0);
    }
    let items = [(1u32, 0.0f32), (2, 1.0), (3, 0.0)];
    for _ in 0..50 {
        assert_eq!(random_element_by_weight(&items, |i| i.1).unwrap().0, 2);
    }
    let empty: [(u32, f32); 0] = [];
    assert!(random_element_by_weight(&empty, |i| i.1).is_none());
}
