/// Legacy JSON fixtures intentionally contain only synthetic test credentials.
/// Production Serialize excludes these fields; tests exercise migration explicitly.
pub fn legacy_config(config: &dbeast::config::Config) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(config)?;
    for (json, source) in value["data_sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(&config.data_sources)
    {
        json["password"] = source.password.clone().into();
        json["ssh"]["password"] = source.ssh.password.clone().into();
        json["ssh"]["passphrase"] = source.ssh.passphrase.clone().into();
    }
    serde_json::to_string(&value)
}
