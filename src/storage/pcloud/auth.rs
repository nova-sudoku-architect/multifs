
/// Run the OAuth flow to get a pCloud access token
///
/// The flow:
/// 1. Open the authorization URL for the user
/// 2. User authorizes in browser
/// 3. We get redirected with a `code` param
/// 4. Exchange code for access_token
///
/// For headless environments, we print the URL and ask the user
/// to paste the redirect URL back.
pub async fn run_oauth_flow(email: &str) -> anyhow::Result<String> {
    let client_id = std::env::var("PCLOUD_APP_CLIENT_ID")
        .map_err(|_| anyhow::anyhow!("PCLOUD_APP_CLIENT_ID not set in environment"))?;
    let client_secret = std::env::var("PCLOUD_APP_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("PCLOUD_APP_CLIENT_SECRET not set in environment"))?;

    // Step 1: Print authorization URL
    let auth_url = format!(
        "https://my.pcloud.com/oauth2/authorize?client_id={}&response_type=code",
        client_id
    );

    println!("========================================");
    println!("pCloud OAuth Authorization for: {}", email);
    println!("========================================");
    println!();
    println!("1. Open this URL in your browser:");
    println!("   {}", auth_url);
    println!();
    println!("2. Log in as '{}'", email);
    println!("3. Authorize the application");
    println!("4. Copy the ENTIRE redirect URL from the browser address bar");
    println!("   (It will start with http://localhost or whatever redirect_uri is configured)");
    println!();
    println!("Paste the redirect URL here:");

    // Read the redirect URL from stdin
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    // Extract the 'code' parameter
    let url = url::Url::parse(input)
        .map_err(|_| anyhow::anyhow!("Invalid URL. Please paste the full redirect URL from the browser."))?;

    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, val)| val.to_string())
        .ok_or_else(|| anyhow::anyhow!("No 'code' parameter found in URL"))?;

    // Step 3: Exchange code for token IMMEDIATELY (codes expire fast)
    println!();
    println!("Exchanging authorization code for token...");

    let exchange_url = format!(
        "https://eapi.pcloud.com/oauth2_token?client_id={}&client_secret={}&code={}",
        client_id, client_secret, code
    );

    let client = reqwest::Client::new();
    let resp = client.get(&exchange_url).send().await?;
    let body: serde_json::Value = resp.json().await?;

    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response: {}", body))?;

    println!();
    println!("✅ Token obtained successfully!");
    println!();
    println!("Add this to your ~/.openclaw/.env file:");
    println!("PCLOUD_TOKEN_{}=\"{}\"", email.to_uppercase().replace('@', "_AT_").replace('-', "_"), access_token);
    println!();
    println!("Then update your multifs config to reference this env var.");
    println!();

    Ok(access_token.to_string())
}
