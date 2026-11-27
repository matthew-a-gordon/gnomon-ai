use std::collections::BTreeMap;

use community_warm_start_proto::{Members, Stored, refresh};

fn stored(communities: &[(u64, &[usize])]) -> Stored {
    Stored {
        communities: communities
            .iter()
            .map(|(id, nodes)| (*id, members(nodes)))
            .collect(),
        next_id: communities.iter().map(|(id, _)| id + 1).max().unwrap_or(0),
    }
}

fn members(nodes: &[usize]) -> Members {
    nodes.iter().copied().collect()
}

fn ids(stored: &Stored) -> BTreeMap<u64, Vec<usize>> {
    stored
        .communities
        .iter()
        .map(|(id, nodes)| (*id, nodes.iter().copied().collect()))
        .collect()
}

#[test]
fn an_unchanged_community_keeps_its_id_and_is_not_regenerated() {
    let before = stored(&[(0, &[0, 1, 2])]);

    let after = refresh(&before, &[7, 7, 7]);

    assert_eq!(ids(&after.stored), BTreeMap::from([(0, vec![0, 1, 2])]));
    assert_eq!(after.regenerated, 0);
}

#[test]
fn a_community_that_keeps_most_members_keeps_its_id_and_is_regenerated() {
    let before = stored(&[(4, &[0, 1, 2, 3])]);

    // Shared 3 of 4 members.
    let after = refresh(&before, &[7, 7, 7, 8]);

    assert_eq!(ids(&after.stored), BTreeMap::from([(4, vec![0, 1, 2])]));
    assert_eq!(after.regenerated, 1);
}

#[test]
fn a_similarity_of_exactly_half_is_no_match() {
    let before = stored(&[(4, &[0, 1, 2])]);

    // Shared 2 of 4 members: {0, 1, 3} against {0, 1, 2}.
    let after = refresh(&before, &[7, 7, 8, 7]);

    // The new Community gets a new ID and the stored one ends. Only the new
    // one counts as regenerated.
    assert_eq!(ids(&after.stored), BTreeMap::from([(5, vec![0, 1, 3])]));
    assert_eq!(after.regenerated, 1);
}

#[test]
fn a_community_of_two_nodes_is_dropped_before_matching() {
    let before = stored(&[(4, &[0, 1, 2])]);

    // {0, 1} shares 2 of 3 members, which would match if it were kept.
    let after = refresh(&before, &[7, 7, 8]);

    assert_eq!(ids(&after.stored), BTreeMap::new());
    assert_eq!(after.regenerated, 0);
}

#[test]
fn an_ended_id_is_never_given_out_again() {
    let before = stored(&[(4, &[0, 1, 2])]);

    let emptied = refresh(&before, &[7, 8, 9]);
    let after = refresh(&emptied.stored, &[7, 7, 7]);

    assert_eq!(ids(&after.stored), BTreeMap::from([(5, vec![0, 1, 2])]));
}

#[test]
fn a_new_id_is_told_apart_from_a_changed_member_set() {
    let before = stored(&[(4, &[0, 1, 2, 3])]);

    // {0, 1, 2} keeps ID 4. {3, 4, 5} is new.
    let after = refresh(&before, &[7, 7, 7, 8, 8, 8]);

    assert_eq!((after.regenerated, after.new_ids), (2, 1));
}
