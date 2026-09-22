// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, Barrier};

use promql_parser::parser::value::ValueType;
use promql_parser::parser::{
    self, Function, FunctionOverridePolicy, FunctionRegistry, FunctionRegistryBuilder,
};

fn custom_fn(
    name: &'static str,
    arg_types: Vec<ValueType>,
    variadic: i32,
    ret: ValueType,
) -> Function {
    Function::new(name, arg_types, variadic, ret, true)
}

fn registry_with(funcs: Vec<Function>) -> FunctionRegistry {
    let mut builder = FunctionRegistryBuilder::new();
    for f in funcs {
        builder.register(f).unwrap();
    }
    builder.build()
}

#[test]
fn test_registry_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FunctionRegistry>();
    assert_send_sync::<FunctionRegistryBuilder>();
}

#[test]
fn test_two_registries_same_text_different_types_barrier() {
    // Two tenants resolve the same function name to different signatures.
    let reg_scalar = registry_with(vec![custom_fn(
        "tenant_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Scalar,
    )]);
    let reg_vector = registry_with(vec![custom_fn(
        "tenant_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Vector,
    )]);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = vec![];
    for (registry, expected) in [
        (reg_scalar, ValueType::Scalar),
        (reg_vector, ValueType::Vector),
    ] {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let expr = parser::parse_with_registry("tenant_fn(1)", &registry).unwrap();
            assert_eq!(expr.value_type(), expected);
            expr.value_type()
        }));
    }
    let types: Vec<ValueType> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_ne!(types[0], types[1]);
}

#[test]
fn test_builder_modification_does_not_affect_existing_snapshot() {
    let base = registry_with(vec![custom_fn(
        "snap_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Scalar,
    )]);
    let snapshot = base.clone();

    // Derive an extended registry from `base`; the derivation must not
    // mutate `base` or its snapshot.
    let mut builder = FunctionRegistryBuilder::from_registry(&base);
    builder
        .register(custom_fn(
            "snap_fn_v2",
            vec![ValueType::Scalar],
            0,
            ValueType::Vector,
        ))
        .unwrap();
    let extended = builder.build();

    // The extended registry resolves both functions.
    assert!(parser::parse_with_registry("snap_fn_v2(1)", &extended).is_ok());
    assert!(parser::parse_with_registry("snap_fn(1)", &extended).is_ok());

    // The original snapshot is unchanged: the new function is unknown there.
    let err = parser::parse_with_registry("snap_fn_v2(1)", &snapshot).unwrap_err();
    assert_eq!(err, "unknown function with name 'snap_fn_v2'");
    // And it still resolves its own function with the original type.
    let expr = parser::parse_with_registry("snap_fn(1)", &snapshot).unwrap();
    assert_eq!(expr.value_type(), ValueType::Scalar);
}

#[test]
fn test_parse_snapshot_isolated_from_concurrent_builder_work() {
    let base = registry_with(vec![custom_fn(
        "iso_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Scalar,
    )]);
    let snapshot = base.clone();

    let barrier = Arc::new(Barrier::new(2));
    let other_barrier = Arc::clone(&barrier);
    let builder_thread = std::thread::spawn(move || {
        other_barrier.wait();
        // Simulate another tenant deriving and building registries while a
        // parse is in flight.
        for i in 0..16 {
            let mut builder = FunctionRegistryBuilder::from_registry(&base);
            let name: &'static str = Box::leak(format!("iso_fn_{i}").into_boxed_str());
            builder
                .register(custom_fn(
                    name,
                    vec![ValueType::Scalar],
                    0,
                    ValueType::Vector,
                ))
                .unwrap();
            let _ = builder.build();
        }
    });

    barrier.wait();
    for _ in 0..16 {
        let expr = parser::parse_with_registry("iso_fn(1)", &snapshot).unwrap();
        assert_eq!(expr.value_type(), ValueType::Scalar);
        let err = parser::parse_with_registry("iso_fn_3(1)", &snapshot).unwrap_err();
        assert_eq!(err, "unknown function with name 'iso_fn_3'");
    }
    builder_thread.join().unwrap();
}

#[test]
fn test_builtin_conflict_rejected_by_default() {
    let mut builder = FunctionRegistryBuilder::new();
    let result = builder.register(custom_fn(
        "rate",
        vec![ValueType::Matrix],
        0,
        ValueType::Scalar,
    ));
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("conflicts with built-in function"));
}

#[test]
fn test_duplicate_custom_function_rejected_by_default() {
    let mut builder = FunctionRegistryBuilder::new();
    builder
        .register(custom_fn("dup_fn", vec![], 0, ValueType::Scalar))
        .unwrap();
    let result = builder.register(custom_fn("dup_fn", vec![], 0, ValueType::Vector));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("already registered"));
}

#[test]
fn test_override_policy_allows_shadowing_builtin() {
    let mut builder =
        FunctionRegistryBuilder::new().with_override_policy(FunctionOverridePolicy::Allow);
    // Shadow the built-in `scalar` (vector -> scalar) with scalar -> vector.
    builder
        .register(custom_fn(
            "scalar",
            vec![ValueType::Scalar],
            0,
            ValueType::Vector,
        ))
        .unwrap();
    let registry = builder.build();

    // The override wins inside this registry only.
    let expr = parser::parse_with_registry("scalar(1)", &registry).unwrap();
    assert_eq!(expr.value_type(), ValueType::Vector);

    // The default registry is unaffected.
    let err = parser::parse_with_registry("scalar(1)", &FunctionRegistry::default());
    assert!(err.is_err());
}

#[test]
fn test_builtin_fallback_in_extended_registry() {
    let registry = registry_with(vec![custom_fn(
        "fallback_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Scalar,
    )]);

    // Built-ins keep working with their original diagnostics.
    let expr = parser::parse_with_registry("rate(foo[5m])", &registry).unwrap();
    assert_eq!(expr.value_type(), ValueType::Vector);
    let err = parser::parse_with_registry("rate(foo)", &registry).unwrap_err();
    assert_eq!(
        err,
        "expected type matrix in call to function 'rate', got vector"
    );
    // Variadic bounds of built-ins are preserved.
    let err = parser::parse_with_registry("round(foo, 1, 2)", &registry).unwrap_err();
    assert_eq!(
        err,
        "expected at most 2 argument(s) in call to 'round', got 3"
    );
    // Unknown functions keep the legacy diagnostic.
    let err = parser::parse_with_registry("no_such_fn(foo)", &registry).unwrap_err();
    assert_eq!(err, "unknown function with name 'no_such_fn'");
    // Function names remain case-sensitive.
    let err = parser::parse_with_registry("RATE(foo[5m])", &registry).unwrap_err();
    assert_eq!(err, "unknown function with name 'RATE'");
}

#[test]
fn test_custom_function_variadic_and_experimental_preserved() {
    let registry = registry_with(vec![custom_fn(
        "variadic_fn",
        vec![ValueType::Vector, ValueType::Scalar],
        1,
        ValueType::Vector,
    )]);

    // Lower bound: missing required arg.
    let err = parser::parse_with_registry("variadic_fn()", &registry).unwrap_err();
    assert_eq!(
        err,
        "expected at least 1 argument(s) in call to 'variadic_fn', got 0"
    );
    // Upper bound: too many args.
    let err = parser::parse_with_registry("variadic_fn(foo, 1, 2)", &registry).unwrap_err();
    assert_eq!(
        err,
        "expected at most 2 argument(s) in call to 'variadic_fn', got 3"
    );
    // Within bounds: experimental flag survives into the AST.
    let expr = parser::parse_with_registry("variadic_fn(foo)", &registry).unwrap();
    match expr {
        parser::Expr::Call(call) => {
            assert!(call.func.experimental);
            assert_eq!(call.func.variadic, 1);
            assert_eq!(call.func.return_type, ValueType::Vector);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn test_zero_config_parse_and_legacy_registration() {
    // Zero-config entry point resolves built-ins without any setup.
    assert!(parser::parse("rate(foo[5m])").is_ok());

    // Legacy global registration only affects the zero-config entry point,
    // not explicit registries.
    parser::register_extra_functions(vec![custom_fn(
        "legacy_scoped_fn",
        vec![ValueType::Scalar],
        0,
        ValueType::Scalar,
    )])
    .unwrap();
    assert!(parser::parse("legacy_scoped_fn(1)").is_ok());
    let err = parser::parse_with_registry("legacy_scoped_fn(1)", &FunctionRegistry::default())
        .unwrap_err();
    assert_eq!(err, "unknown function with name 'legacy_scoped_fn'");

    parser::clear_extra_functions();
    let err = parser::parse("legacy_scoped_fn(1)").unwrap_err();
    assert_eq!(err, "unknown function with name 'legacy_scoped_fn'");
}

#[cfg(feature = "ser")]
#[test]
fn test_registry_parse_serde_round_trip() {
    let registry = registry_with(vec![custom_fn(
        "serde_fn",
        vec![ValueType::Matrix],
        0,
        ValueType::Vector,
    )]);
    let ast = parser::parse_with_registry("serde_fn(foo[5m])", &registry).unwrap();

    let json = serde_json::to_value(&ast).expect("Failed to serialize");
    assert_eq!(json["func"]["name"], serde_json::json!("serde_fn"));
    assert_eq!(json["func"]["variadic"], serde_json::json!(0));
    assert_eq!(json["func"]["returnType"], serde_json::json!("vector"));

    // Round-trip through a JSON string is stable.
    let text = serde_json::to_string(&json).expect("Failed to stringify");
    let back: serde_json::Value = serde_json::from_str(&text).expect("Failed to deserialize");
    assert_eq!(json, back);
}
