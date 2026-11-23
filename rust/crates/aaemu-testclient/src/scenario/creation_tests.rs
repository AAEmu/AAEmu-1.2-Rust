use super::character_to_select;
use crate::game::CharInfo;

#[test]
fn fresh_creation_selects_the_new_character_and_never_falls_back_to_existing() {
    let mut characters = vec![CharInfo {
        id: 1,
        name: "Existing".into(),
        appearance: Default::default(),
    }];
    assert!(character_to_select(&characters, "Fresh", true).is_err());
    characters.push(CharInfo {
        id: 2,
        name: "Fresh".into(),
        appearance: Default::default(),
    });
    assert_eq!(
        character_to_select(&characters, "fresh", true).unwrap().id,
        2
    );
    assert_eq!(
        character_to_select(&characters, "Fresh", false).unwrap().id,
        1
    );
}
