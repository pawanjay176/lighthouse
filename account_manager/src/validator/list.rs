use crate::VALIDATOR_DIR_FLAG;
use clap::App;
use std::path::PathBuf;
use validator_dir::Manager as ValidatorManager;

pub const CMD: &str = "list";

pub fn cli_app<'a, 'b>() -> App<'a, 'b> {
    App::new(CMD)
}

pub fn cli_run(validator_dir: PathBuf) -> Result<(), String> {
    let mgr = ValidatorManager::open(&validator_dir)
        .map_err(|e| format!("Unable to read --{}: {:?}", VALIDATOR_DIR_FLAG, e))?;

    for (name, _path) in mgr
        .directory_names()
        .map_err(|e| format!("Unable to list validators: {:?}", e))?
    {
        println!("{}", name)
    }

    Ok(())
}
