pub fn prompt_and_store_secret(
    provisional_secret_id: &str,
    client_name: &str,
    label: &str,
    description: &str,
    env_var: Option<&str>,
    replacing: bool,
    workspace: &str,
) -> Result<bool, String> {
    crate::native_ui::prompt_and_store_secret(
        provisional_secret_id,
        client_name,
        label,
        description,
        env_var,
        replacing,
        workspace,
    )
}

pub fn confirm(client_name: &str, title: &str, message: &str) -> Result<bool, String> {
    crate::native_ui::confirm(client_name, title, message)
}
