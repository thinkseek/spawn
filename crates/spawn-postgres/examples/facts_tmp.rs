//! Temporary: print the dispatch facts of the database in SPAWN_TEST_DATABASE_URL.
fn main() {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect("url");
    let store = spawn_postgres::PostgresStore::connect(&url).expect("connect");
    let facts = store.dispatch_facts().expect("facts");
    println!("{facts}");
}
