#[path = "../build_support/text_key_parser.rs"]
mod text_key_parser;

use std::path::Path;
use text_key_parser::parse_text_keys_from_source;

#[test]
fn text_key_ast_parser_accepts_comments_attributes_and_discriminants() {
    let source = r#"
        #[repr(usize)]
        pub enum TextKey {
            // first key
            AppTitle = 0,
            #[allow(dead_code)]
            RunTitle,
            RunPrompt,
        }
    "#;

    assert_eq!(
        parse_text_keys_from_source(source, Path::new("text_key.rs")),
        ["AppTitle", "RunTitle", "RunPrompt"]
    );
}
