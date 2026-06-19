use deductive_check::core_provider::IOProvider;
use deductive_check::provider::{CliRustAnalyzerProvider, RustAnalyzerRequest, RustAnalyzerResponse};
use std::path::PathBuf;

fn duv_demo_path() -> PathBuf {
    PathBuf::from("/workspace/duv_demo")
}

fn duv_demo_src_path(file: &str) -> PathBuf {
    duv_demo_path().join("src").join(file)
}

async fn create_provider() -> CliRustAnalyzerProvider {
    CliRustAnalyzerProvider::new(duv_demo_path().to_string_lossy().to_string())
        .expect("Failed to create provider for duv_demo")
}

async fn list_functions(provider: &CliRustAnalyzerProvider, file: &str) -> Vec<deductive_check::provider::FunctionInfo> {
    let path = duv_demo_src_path(file);
    let resp = provider
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: vec![path],
            cfg_verification: true,
        })
        .await
        .expect("ListFunctions failed");

    match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => panic!("Expected FunctionList"),
    }
}

#[tokio::test]
async fn test_duv_demo_lists_invariant_impl_methods() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let inc = functions.iter().find(|f| f.id.name == "inc");
    assert!(inc.is_some(), "inc() should be listed, got: {:?}", functions.iter().map(|f| &f.id.name).collect::<Vec<_>>());

    let inc_fn = inc.unwrap();
    assert!(inc_fn.has_guarantees, "inc() in #[invariant] impl should have guarantees");
    assert!(inc_fn.id.impl_for.is_some(), "inc() should have impl_for");

    let new_fn = functions.iter().find(|f| f.id.name == "new");
    assert!(new_fn.is_some(), "new() should be listed");
    assert!(new_fn.unwrap().has_guarantees, "new() in #[invariant] impl should have guarantees");
}

#[tokio::test]
async fn test_duv_demo_nested_loops_calls_precondition_fn() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let nested = functions.iter().find(|f| f.id.name == "nested_loops_5");
    assert!(nested.is_some(), "nested_loops_5 should be listed");

    let nested_fn = nested.unwrap();
    assert!(nested_fn.has_guarantees, "nested_loops_5 should have guarantees because it calls inc() which has preconditions (invariant impl)");
}

#[tokio::test]
async fn test_duv_demo_runner_functions() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "runner.rs").await;

    let names: Vec<String> = functions.iter().map(|f| f.id.name.clone()).collect();
    assert!(names.contains(&"nested_loops_4".to_string()), "nested_loops_4 should be listed, got: {:?}", names);
    assert!(names.contains(&"nested_loops_2".to_string()), "nested_loops_2 should be listed, got: {:?}", names);

    for f in &functions {
        if f.id.name == "nested_loops_4" || f.id.name == "nested_loops_2" {
            assert!(f.has_guarantees, "{} should have guarantees (calls inc with preconditions)", f.id.name);
        }
    }
}

#[tokio::test]
async fn test_duv_demo_get_function_code() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let inc = functions.iter().find(|f| f.id.name == "inc").expect("inc not found");

    let resp = provider
        .invoke(RustAnalyzerRequest::GetFunctionCode {
            function_id: inc.id.clone(),
        })
        .await
        .expect("GetFunctionCode failed");

    let code = match resp {
        RustAnalyzerResponse::FunctionCode(code) => code,
        _ => panic!("Expected FunctionCode"),
    };

    assert!(code.contains("self.value"), "inc() code should contain self.value, got: {}", code);
}

#[tokio::test]
async fn test_duv_demo_called_functions() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let nested = functions.iter().find(|f| f.id.name == "nested_loops_5").expect("nested_loops_5 not found");

    let resp = provider
        .invoke(RustAnalyzerRequest::GetCalledFunctions {
            function_id: nested.id.clone(),
        })
        .await
        .expect("GetCalledFunctions failed");

    let called = match resp {
        RustAnalyzerResponse::CalledFunctionList(fns) => fns,
        _ => panic!("Expected CalledFunctionList"),
    };

    let called_names: Vec<&str> = called.iter().map(|c| c.name.as_str()).collect();
    assert!(
        called_names.iter().any(|n| *n == "inc" || *n == "Value::inc"),
        "nested_loops_5 should call inc, got: {:?}",
        called_names
    );
}

#[tokio::test]
async fn test_duv_demo_called_function_code() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let nested = functions.iter().find(|f| f.id.name == "nested_loops_5").expect("nested_loops_5 not found");

    let resp = provider
        .invoke(RustAnalyzerRequest::GetCalledFunctions {
            function_id: nested.id.clone(),
        })
        .await
        .expect("GetCalledFunctions failed");

    let called = match resp {
        RustAnalyzerResponse::CalledFunctionList(fns) => fns,
        _ => panic!("Expected CalledFunctionList"),
    };

    let inc_call = called.iter().find(|c| c.name == "inc" || c.name == "Value::inc");
    if let Some(inc_called) = inc_call {
        let resp = provider
            .invoke(RustAnalyzerRequest::GetCalledFunctionCode {
                called: inc_called.clone(),
            })
            .await
            .expect("GetCalledFunctionCode failed");

        let result = match resp {
            RustAnalyzerResponse::CalledFunctionCode(r) => r,
            _ => panic!("Expected CalledFunctionCode"),
        };

        assert!(
            !result.code.starts_with("// Could not find"),
            "inc() code should be found, got: {}",
            result.code
        );
        assert!(
            result.code.contains("self.value"),
            "inc() called function code should contain self.value, got: {}",
            result.code
        );
    }
}

#[tokio::test]
async fn test_duv_demo_function_docs_api() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let inc = functions.iter().find(|f| f.id.name == "inc").expect("inc not found");

    let resp = provider
        .invoke(RustAnalyzerRequest::GetFunctionDocs {
            function_id: inc.id.clone(),
        })
        .await
        .expect("GetFunctionDocs failed");

    match resp {
        RustAnalyzerResponse::FunctionDocs(_) => {}
        _ => panic!("Expected FunctionDocs"),
    }
}

#[tokio::test]
async fn test_duv_demo_no_test_functions() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    for f in &functions {
        assert!(
            !f.id.name.starts_with("test_"),
            "Test functions should be excluded, found: {}",
            f.id.name
        );
    }
}

#[tokio::test]
async fn test_duv_demo_impl_for_set_for_methods() {
    let provider = create_provider().await;
    let functions = list_functions(&provider, "counter.rs").await;

    let impl_methods: Vec<_> = functions.iter().filter(|f| f.id.impl_for.is_some()).collect();
    assert!(!impl_methods.is_empty(), "Should find impl methods (inc, new) with impl_for");

    for m in &impl_methods {
        let impl_for = m.id.impl_for.as_ref().unwrap();
        assert!(
            impl_for.contains("Value"),
            "Impl method {} should have impl_for containing 'Value', got: {}",
            m.id.name,
            impl_for
        );
    }
}
