use oncall_ai::model::{ModelSpec, provider_names};

#[test]
fn model_spec_preserves_colons_after_the_provider_separator() {
    let spec: ModelSpec = "ollama:qwen3:8b".parse().expect("valid model spec");

    assert_eq!(spec.provider().as_str(), "ollama");
    assert_eq!(spec.model().as_str(), "qwen3:8b");
}

#[test]
fn model_spec_accepts_provider_qualified_model_paths() {
    let spec: ModelSpec = "openrouter:openai/gpt-5".parse().expect("valid model spec");

    assert_eq!(spec.to_string(), "openrouter:openai/gpt-5");
}

#[test]
fn empty_model_identifier_is_rejected() {
    let error = "ollama:"
        .parse::<ModelSpec>()
        .expect_err("empty model must fail");

    assert!(error.to_string().contains("model identifier is empty"));
}

#[test]
fn unknown_provider_reports_the_single_sorted_registry() {
    let error = "voyageai:voyage-3"
        .parse::<ModelSpec>()
        .expect_err("non-completion provider must fail");
    let names = provider_names();

    assert_eq!(names.len(), 24);
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(error.to_string().contains(&names.join(", ")));
}
