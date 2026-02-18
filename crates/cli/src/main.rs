use anyhow::Result;

fn main() -> Result<()> {
    println!("VaultAgent CLI (MVP scaffold)");
    println!("core status: {}", vaultagent_core::health());
    Ok(())
}
