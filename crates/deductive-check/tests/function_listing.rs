use deductive_check::core_provider::IOProvider;
use deductive_check::provider::{CliRustAnalyzerProvider, RustAnalyzerRequest, RustAnalyzerResponse};
use std::path::PathBuf;

fn test_dummy_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test_dummy")
}

fn listed_function_names(functions: &[deductive_check::provider::FunctionInfo]) -> Vec<String> {
    let mut names: Vec<String> = functions.iter().map(|f| f.id.name.clone()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn test_excludes_test_functions() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let names = listed_function_names(&functions);

    assert!(
        !names.contains(&"test_direct".to_string()),
        "#[test] function should be excluded, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"test_tokio".to_string()),
        "#[tokio::test] function should be excluded, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"test_log_tokio".to_string()),
        "#[test_log::test(tokio::test)] function should be excluded, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"test_in_cfg_test_mod".to_string()),
        "#[cfg(test)] mod function should be excluded, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"tokio_in_cfg_test_mod".to_string()),
        "#[tokio::test] in #[cfg(test)] should be excluded, got: {:?}",
        names
    );

    assert!(
        names.contains(&"regular_function".to_string()),
        "regular_function should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"with_unsafe_block".to_string()),
        "function with unsafe block should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"with_assertion".to_string()),
        "function with assertion should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"with_docs_guarantee".to_string()),
        "function with doc guarantees should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"increment".to_string()),
        "impl method should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"raw_ptr".to_string()),
        "unsafe impl method should be listed, got: {:?}",
        names
    );
    assert!(
        names.contains(&"process".to_string()),
        "trait impl method should be listed, got: {:?}",
        names
    );
}

#[tokio::test]
async fn test_verification_feature_functions() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let names = listed_function_names(&functions);

    assert!(
        names.contains(&"verification_only".to_string()),
        "#[cfg(verification)] function should be listed (feature enabled), got: {:?}",
        names
    );
}

#[tokio::test]
async fn test_has_guarantees_detection() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let guaranteed: Vec<&str> = functions
        .iter()
        .filter(|f| f.has_guarantees)
        .map(|f| f.id.name.as_str())
        .collect();

    assert!(
        guaranteed.contains(&"with_unsafe_block"),
        "function with unsafe block should have guarantees, got: {:?}",
        guaranteed
    );
    assert!(
        guaranteed.contains(&"with_assertion"),
        "function with assert! should have guarantees, got: {:?}",
        guaranteed
    );
    assert!(
        guaranteed.contains(&"with_docs_guarantee"),
        "function with # Guarantees docs should have guarantees, got: {:?}",
        guaranteed
    );

    let not_guaranteed: Vec<&str> = functions
        .iter()
        .filter(|f| !f.has_guarantees)
        .map(|f| f.id.name.as_str())
        .collect();

    assert!(
        not_guaranteed.contains(&"regular_function"),
        "plain function should NOT have guarantees, got guaranteed: {:?}",
        guaranteed
    );
    assert!(
        not_guaranteed.contains(&"raw_ptr"),
        "unsafe fn (no unsafe block) should NOT have guarantees, got guaranteed: {:?}",
        guaranteed
    );
}

#[tokio::test]
async fn test_impl_for_detection() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let counter_methods: Vec<_> = functions
        .iter()
        .filter(|f| f.id.name == "increment" || f.id.name == "raw_ptr")
        .collect();

    for method in &counter_methods {
        assert!(
            method.id.impl_for.is_some(),
            "Counter method {} should have impl_for, got None",
            method.id.name
        );
        let impl_for = method.id.impl_for.as_ref().unwrap();
        assert!(
            impl_for.contains("Counter"),
            "Counter method impl_for should contain 'Counter', got: {}",
            impl_for
        );
    }

    let trait_impl: Vec<_> = functions
        .iter()
        .filter(|f| f.id.name == "process")
        .collect();

    for method in &trait_impl {
        assert!(
            method.id.impl_for.is_some(),
            "trait impl method should have impl_for"
        );
    }
}

#[tokio::test]
async fn test_fetch_docs_returns_doc_comments() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let with_docs_guarantee = functions.iter().find(|f| f.id.name == "with_docs_guarantee")
        .expect("should find with_docs_guarantee");
    assert!(
        with_docs_guarantee.docs.contains("Guarantees"),
        "docs should contain 'Guarantees', got: {:?}", with_docs_guarantee.docs
    );

    let read_ptr = functions.iter().find(|f| f.id.name == "read_ptr")
        .expect("should find read_ptr");
    assert!(
        read_ptr.docs.contains("Safety"),
        "read_ptr docs should contain 'Safety', got: {:?}", read_ptr.docs
    );
}

#[tokio::test]
async fn test_fetch_docs_includes_doc_attrs() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let with_doc_attr = functions.iter().find(|f| f.id.name == "with_doc_attr")
        .expect("should find with_doc_attr");
    assert!(
        with_doc_attr.docs.contains("Ensures"),
        "doc attr function docs should contain 'Ensures', got: {:?}", with_doc_attr.docs
    );
}

#[tokio::test]
async fn test_has_guarantees_via_outgoing_calls() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let calls_precondition_fn = functions.iter().find(|f| f.id.name == "calls_precondition_fn")
        .expect("should find calls_precondition_fn");
    assert!(
        calls_precondition_fn.has_guarantees,
        "calls_precondition_fn should have guarantees because it calls with_preconditions which has # Preconditions in docs, got: has_guarantees={}, docs={:?}",
        calls_precondition_fn.has_guarantees,
        calls_precondition_fn.docs,
    );

    let safe_read = functions.iter().find(|f| f.id.name == "safe_read")
        .expect("should find safe_read");
    assert!(
        safe_read.has_guarantees,
        "safe_read should have guarantees (has unsafe block + calls unsafe fn with preconditions), got: has_guarantees={}",
        safe_read.has_guarantees,
    );
}

#[tokio::test]
async fn test_preconditions_detected_in_docs() {
    let path = test_dummy_path();
    let provider = CliRustAnalyzerProvider::new(path.to_string_lossy().to_string())
        .expect("Failed to create provider for test_dummy");

    let lib_path = path.join("src").join("lib.rs");
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![lib_path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    };

    let with_preconditions = functions.iter().find(|f| f.id.name == "with_preconditions")
        .expect("should find with_preconditions");
    assert!(
        with_preconditions.docs.contains("Preconditions"),
        "with_preconditions docs should contain 'Preconditions', got: {:?}", with_preconditions.docs
    );
}
