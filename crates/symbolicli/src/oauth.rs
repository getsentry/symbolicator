use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AccessToken, AuthUrl, AuthorizationCode, ClientId, CsrfToken, DeviceAuthorizationUrl,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};

use keyring::Entry;

use reqwest::StatusCode;

use std::collections::HashMap;
use std::env;
use std::process::Command;
use std::sync::Arc;

const CLIENT_ID: &str = "0oa87t4d8dCJbeZuG5d7";

pub async fn authenticate() -> anyhow::Result<()> {
    let device_auth_url = DeviceAuthorizationUrl::new(
        "https://dev-50022714.okta.com/oauth2/default/v1/device/authorize".to_string(),
    )?;
    let auth_url =
        AuthUrl::new("https://dev-50022714.okta.com/oauth2/default/v1/authorize".to_string())?;
    let token_url =
        TokenUrl::new("https://dev-50022714.okta.com/oauth2/default/v1/token".to_string())?;
    let redirect_url = RedirectUrl::new("http://localhost:8085/".to_string())?;

    // Generate a PKCE challenge.
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let client = BasicClient::new(
        ClientId::new(CLIENT_ID.to_string()),
        None, // we do not pass a client secret
        auth_url,
        Some(token_url),
    )
    .set_device_authorization_url(device_auth_url)
    .set_redirect_uri(redirect_url);

    let (authorize_url, csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("okta.myAccount.email.read".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    println!(
        "Open this URL in your browser:\n{}\n",
        authorize_url.to_string()
    );

    let _ = Command::new("open")
        .arg(authorize_url.to_string())
        .output()?;

    let server_state = ServerState {
        expected_csrf_token: csrf_state,
        client,
        pkce_verifier,
    };

    let app = Router::new()
        .route("/", get(handle_token_request))
        .with_state(Arc::new(server_state));

    axum::Server::bind(&"127.0.0.1:8085".parse().unwrap())
        .serve(app.into_make_service())
        .await?;

    // pulled from https://github.com/ramosbugs/oauth2-rs/blob/main/examples/github.rs, but we should
    // separate this out into its own function and use something more robust
    // let listener = TcpListener::bind("127.0.0.1:8085").unwrap();
    // for stream in listener.incoming() {
    //     if let Ok(mut stream) = stream {
    //         let code;
    //         let state;
    //         {
    //             println!("Server listening...");
    //             let mut reader = BufReader::new(&stream);

    //             let mut request_line = String::new();
    //             reader.read_line(&mut request_line).unwrap();

    //             dbg!(&request_line);
    //             let redirect_url = request_line.split_whitespace().nth(1).unwrap();
    //             let url = Url::parse(&("http://localhost".to_string() + redirect_url)).unwrap();

    //             let code_pair = url
    //                 .query_pairs()
    //                 .find(|pair| {
    //                     let &(ref key, _) = pair;
    //                     key == "code"
    //                 })
    //                 .unwrap();

    //             let (_, value) = code_pair;
    //             code = AuthorizationCode::new(value.into_owned());

    //             let state_pair = url
    //                 .query_pairs()
    //                 .find(|pair| {
    //                     let &(ref key, _) = pair;
    //                     key == "state"
    //                 })
    //                 .unwrap();

    //             let (_, value) = state_pair;
    //             state = CsrfToken::new(value.into_owned());
    //         }

    //         let message = "Go back to your terminal :)";
    //         let response = format!(
    //             "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
    //             message.len(),
    //             message
    //         );
    //         stream.write_all(response.as_bytes()).unwrap();

    //         println!("Okta returned the following code:\n{}\n", code.secret());
    //         println!(
    //             "Okta returned the following state:\n{} (expected `{}`)\n",
    //             state.secret(),
    //             csrf_state.secret()
    //         );

    //         // Exchange the code with a token.
    //         let token_res = client
    //             .exchange_code(code)
    //             .set_pkce_verifier(pkce_verifier)
    //             .request(http_client);

    //         println!("Okta returned the following token:\n{:?}\n", token_res);

    //         if let Ok(token) = token_res {
    //             // NB: Github returns a single comma-separated "scope" parameter instead of multiple
    //             // space-separated scopes. Github-specific clients can parse this scope into
    //             // multiple scopes by splitting at the commas. Note that it's not safe for the
    //             // library to do this by default because RFC 6749 allows scopes to contain commas.
    //             let scopes = if let Some(scopes_vec) = token.scopes() {
    //                 scopes_vec
    //                     .iter()
    //                     .map(|comma_separated| comma_separated.split(','))
    //                     .flatten()
    //                     .collect::<Vec<_>>()
    //             } else {
    //                 Vec::new()
    //             };
    //             println!("Okta returned the following scopes:\n{:?}\n", scopes);
    //         }

    //         // The server will terminate itself after collecting the first code.
    //         break;
    //     }
    // }

    Ok(())
}

fn store_token_in_keyring(access_token: &AccessToken) -> keyring::Result<()> {
    let entry = Entry::new(
        "symbolicli",
        env!("USER", "Unable to read USER environment variable"),
    )?;
    println!("Storing token in system secure keyring...");
    entry.set_password(access_token.secret())?;

    println!("Getting token from keyring...");
    let secret_token = entry.get_password()?;
    println!("Secret token is '{}'", secret_token);

    Ok(())
}

#[derive(Debug)]
struct ServerState {
    expected_csrf_token: CsrfToken,
    client: BasicClient,
    pkce_verifier: PkceCodeVerifier,
}

async fn handle_token_request(
    Query(params): Query<HashMap<String, String>>,
    State(server_state): State<Arc<ServerState>>,
) -> Result<String, StatusCode> {
    let Some(code )= params.get("code") else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(state)= params.get("state") else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let code = AuthorizationCode::new(code.to_string());
    let state = CsrfToken::new(state.to_string());

    // TODO: possibly constant time comparison?
    if state.secret() != server_state.expected_csrf_token.secret() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Exchange the code with a token.
    let token_res = server_state
        .client
        .exchange_code(code)
        .set_pkce_verifier(PkceCodeVerifier::new(
            server_state.pkce_verifier.secret().clone(),
        ))
        .request_async(async_http_client)
        .await;

    println!("Okta returned the following token:\n{token_res:?}\n");

    store_token_in_keyring(token_res.unwrap().access_token()).unwrap();

    Ok("Go back to your terminal :)".to_string())
}
