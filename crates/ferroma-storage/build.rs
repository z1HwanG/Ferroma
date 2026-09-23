fn main() {
    // sqlx::migrate! embeds the migration list, but a newly added SQL file may
    // otherwise leave an incremental build using the old embedded list.
    println!("cargo:rerun-if-changed=../../migrations");
}
