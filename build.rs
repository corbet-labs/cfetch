fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let major = version
        .split('.')
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .expect("package version has a numeric major component");
    assert!(
        major == 0,
        "cfetch 1.0 and every later major are operator-blocked; do not remove this guard without Julian's explicit instruction"
    );
}
