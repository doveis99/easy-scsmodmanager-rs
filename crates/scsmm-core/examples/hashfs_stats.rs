//! Temporary diagnostic: entries in an archive vs. what the listings reach.
use scsmm_core::scs::hashfs::HashFsReader;
fn main() {
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        match HashFsReader::open(path) {
            Ok(mut r) => {
                let (all, dirs, images) = r.entry_counts();
                let listed = r.iter_files();
                let root = r.list_dir("/").map(|(d, f)| format!("root listing: {} dirs, {} files", d.len(), f.len())).unwrap_or_else(|e| format!("no root listing ({e})"));
                println!("{}: entries={all} dir-listings={dirs} packed-textures={images} reachable-files={} | {root}", path.file_name().unwrap().to_string_lossy(), listed.len());
            }
            Err(e) => println!("{arg}: {e}"),
        }
    }
}
