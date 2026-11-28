//! The recipes this repository keeps (`recipes/*.toml`) parse and validate, as
//! `hgl-ai build recipe import` would check them.

use hgl_ai_build::recipe::Recipe;

#[test]
fn every_recipe_here_validates() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "toml") {
            let text = std::fs::read_to_string(&p).unwrap();
            let r = Recipe::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            r.validate()
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            n += 1;
        }
    }
    assert!(n >= 3, "{n} recipes");
}
