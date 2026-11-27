use community_warm_start_proto::keeps_warm_start;

#[test]
fn the_warm_start_is_kept_at_exactly_a_third_fewer() {
    assert!(keeps_warm_start(200, 300));
}

#[test]
fn the_warm_start_is_dropped_just_short_of_a_third_fewer() {
    assert!(!keeps_warm_start(201, 300));
    // A third of 100 is 33.3, so 67 is short of it and 66 is not.
    assert!(!keeps_warm_start(67, 100));
    assert!(keeps_warm_start(66, 100));
}
