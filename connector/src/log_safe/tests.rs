use super::*;

#[test]
fn a_conversation_id_is_named_by_the_shared_hash() {
    assert_eq!(
        IdHash("cursor-editor-00000000-0000-0000-0000-000000000000").to_string(),
        "h:64b0598f2d9d"
    );
}

#[test]
fn a_client_id_outside_the_grammar_is_invalid() {
    assert_eq!(ClientId("turn-1.a:b_c").to_string(), "turn-1.a:b_c");
    for bad in ["", "two words", "line\nbreak", &"x".repeat(65), "naïve"] {
        assert_eq!(ClientId(bad).to_string(), "invalid", "{bad:?}");
    }
}

#[test]
fn the_home_is_shown_as_a_tilde() {
    let home = std::path::Path::new("/home/ana");
    let shown = |path: &str| {
        HomePath {
            path: path.as_ref(),
            home,
        }
        .to_string()
    };
    assert_eq!(shown("/home/ana"), "~");
    assert_eq!(shown("/home/ana/x/y").replace('\\', "/"), "~/x/y");
    assert_eq!(shown("/srv/z"), "/srv/z");
}

#[test]
fn an_error_is_a_code_never_its_text() {
    let io =
        anyhow::Error::from(std::io::Error::other("/home/canary-user-7f3a")).context("wrapped");
    assert_eq!(error_code(&io), "io");
    assert_eq!(
        error_code(&anyhow::anyhow!("/home/canary-user-7f3a")),
        "error"
    );
}
