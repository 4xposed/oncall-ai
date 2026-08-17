use std::collections::BTreeMap;

use oncall_ai::model::{
    EnvironmentError, EnvironmentSource, ModelRuntime, ModelSpec, ProviderConfigs,
};
use rig_core::completion::CompletionModel as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Default)]
struct TestEnvironment(BTreeMap<String, String>);

impl EnvironmentSource for TestEnvironment {
    fn var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
        Ok(self.0.get(name).cloned())
    }
}

#[test]
fn every_registered_provider_builds_locally_without_a_probe() {
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({}))
        .expect("empty provider blocks parse");
    let environment = TestEnvironment(BTreeMap::from([
        ("ANTHROPIC_API_KEY".to_owned(), "test-key".to_owned()),
        ("AZURE_API_KEY".to_owned(), "test-key".to_owned()),
        ("AZURE_API_VERSION".to_owned(), "2026-01-01".to_owned()),
        (
            "AZURE_ENDPOINT".to_owned(),
            "https://example.openai.azure.com".to_owned(),
        ),
        ("COHERE_API_KEY".to_owned(), "test-key".to_owned()),
        ("DEEPSEEK_API_KEY".to_owned(), "test-key".to_owned()),
        ("DOUBLEWORD_API_KEY".to_owned(), "test-key".to_owned()),
        ("GEMINI_API_KEY".to_owned(), "test-key".to_owned()),
        ("GROQ_API_KEY".to_owned(), "test-key".to_owned()),
        ("HUGGINGFACE_API_KEY".to_owned(), "test-key".to_owned()),
        ("HYPERBOLIC_API_KEY".to_owned(), "test-key".to_owned()),
        (
            "LLAMAFILE_API_BASE_URL".to_owned(),
            "http://localhost:8080".to_owned(),
        ),
        ("MINIMAX_API_KEY".to_owned(), "test-key".to_owned()),
        ("MIRA_API_KEY".to_owned(), "test-key".to_owned()),
        ("MISTRAL_API_KEY".to_owned(), "test-key".to_owned()),
        ("MOONSHOT_API_KEY".to_owned(), "test-key".to_owned()),
        ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
        ("OPENROUTER_API_KEY".to_owned(), "test-key".to_owned()),
        ("PERPLEXITY_API_KEY".to_owned(), "test-key".to_owned()),
        ("TOGETHER_API_KEY".to_owned(), "test-key".to_owned()),
        ("XAI_API_KEY".to_owned(), "test-key".to_owned()),
        ("XIAOMI_MIMO_API_KEY".to_owned(), "test-key".to_owned()),
        ("ZAI_API_KEY".to_owned(), "test-key".to_owned()),
    ]));
    let specs = oncall_ai::model::provider_names()
        .iter()
        .map(|provider| format!("{provider}:test-model").parse::<ModelSpec>())
        .collect::<Result<Vec<_>, _>>()
        .expect("registry names form valid specs");

    let runtime = ModelRuntime::build(&providers, &specs, &environment)
        .expect("all canonical clients construct without network access");

    for spec in &specs {
        runtime.model(spec).expect("referenced model is cached");
    }
}

#[test]
fn azure_rejects_conflicting_auth_before_client_construction() {
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
        "azure": {
            "api_key": {"value": "test-key"},
            "token": {"value": "test-token"},
            "endpoint": "https://example.openai.azure.com",
            "api_version": "2026-01-01"
        }
    }))
    .expect("azure provider block parses");
    let specs = vec!["azure:test-model".parse::<ModelSpec>().expect("valid spec")];

    let error = ModelRuntime::build(&providers, &specs, &TestEnvironment::default())
        .expect_err("conflicting auth must fail locally");

    assert!(error.to_string().contains("exactly one"), "{error}");
}

#[test]
fn explicit_azure_auth_choice_overrides_the_other_environment_choice() {
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
        "azure": {
            "api_key": {"value": "config-key"},
            "endpoint": "https://example.openai.azure.com",
            "api_version": "2026-01-01"
        }
    }))
    .expect("azure provider block parses");
    let specs = vec!["azure:test-model".parse::<ModelSpec>().expect("valid spec")];
    let environment = TestEnvironment(BTreeMap::from([(
        "AZURE_TOKEN".to_owned(),
        "environment-token".to_owned(),
    )]));

    ModelRuntime::build(&providers, &specs, &environment)
        .expect("explicit API key suppresses conventional token fallback");
}

#[test]
fn azure_rejects_an_empty_api_version() {
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
        "azure": {
            "api_key": {"value": "test-key"},
            "endpoint": "https://example.openai.azure.com",
            "api_version": ""
        }
    }))
    .expect("azure provider block parses");
    let specs = vec!["azure:test-model".parse::<ModelSpec>().expect("valid spec")];

    let error = ModelRuntime::build(&providers, &specs, &TestEnvironment::default())
        .expect_err("empty API version must fail locally");

    assert!(error.to_string().contains("api_version"), "{error}");
}

#[test]
fn invalid_configured_provider_fails_even_when_unreferenced() {
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
        "openai": {
            "api_key": {"value": "test-key"},
            "base_url": "localhost:8080"
        }
    }))
    .expect("provider block parses");
    let specs = vec![
        "ollama:test-model"
            .parse::<ModelSpec>()
            .expect("valid spec"),
    ];

    let error = ModelRuntime::build(&providers, &specs, &TestEnvironment::default())
        .expect_err("every explicitly configured provider must be validated");

    assert!(
        error.to_string().contains("providers.openai.base_url"),
        "{error}"
    );
}

#[tokio::test]
async fn explicit_chatgpt_token_uses_conventional_account_id_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake ChatGPT listener binds");
    let address = listener.local_addr().expect("listener has address");
    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let mut bytes = vec![0; 16_384];
        let length = socket.read(&mut bytes).await.expect("read request");
        let request = String::from_utf8_lossy(&bytes[..length]).into_owned();
        request_tx.send(request).expect("test receives request");
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("write response");
    });
    let providers = serde_json::from_value::<ProviderConfigs>(serde_json::json!({
        "chatgpt": {
            "base_url": format!("http://{address}"),
            "auth": {
                "type": "access_token",
                "secret": {"value": "test-token"}
            }
        }
    }))
    .expect("ChatGPT provider block parses");
    let spec = "chatgpt:test-model"
        .parse::<ModelSpec>()
        .expect("valid spec");
    let environment = TestEnvironment(BTreeMap::from([(
        "CHATGPT_ACCOUNT_ID".to_owned(),
        "environment-account".to_owned(),
    )]));
    let runtime = ModelRuntime::build(&providers, std::slice::from_ref(&spec), &environment)
        .expect("ChatGPT client builds");
    let model = runtime.model(&spec).expect("model was constructed");

    let _error = model
        .completion(model.completion_request("test").build())
        .await
        .expect_err("fake server returns 500");
    let request = request_rx.await.expect("captured request").to_lowercase();

    assert!(
        request.contains("chatgpt-account-id: environment-account"),
        "request must carry conventional account ID fallback:\n{request}"
    );
}
