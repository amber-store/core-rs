//! Probe for interop/check.sh: repair packs written by the other implementation.
use amber_store_core::key::{Key, Type};
use amber_store_core::packstore::{Options, Store};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let store = Store::open_with(&args[2], Options::default().segment_size(1))?;
    for data in [
        b"repair target".as_slice(),
        b"unrelated survivor".as_slice(),
    ] {
        let key = Key::new(Type::Blob, data.len() as u64, data);
        match args[1].as_str() {
            "create" => store.put(key, data)?,
            "repair" => store.put_verified(key, data)?,
            "check" => assert_eq!(store.get(key)?, data),
            _ => panic!("expected create, repair, or check"),
        }
    }
    store.verify(|| false)?;
    store.close()?;
    Ok(())
}
