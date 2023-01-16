/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_common::executor_config::PathSeparatorKind;
use buck2_common::package_listing::listing::testing::PackageListingExt;
use buck2_common::package_listing::listing::PackageListing;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::cells::testing::CellResolverExt;
use buck2_core::cells::CellName;
use buck2_core::cells::CellResolver;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::project::ProjectRelativePathBuf;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::package::PackageLabel;
use buck2_execute::artifact::fs::ArtifactFs;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_execute::path::buck_out_path::BuckOutPathResolver;
use buck2_execute::path::buck_out_path::BuckPathResolver;
use buck2_interpreter_for_build::attrs::coerce::attr_type::AttrTypeExt;
use buck2_interpreter_for_build::attrs::coerce::testing::coercion_ctx;
use buck2_interpreter_for_build::attrs::coerce::testing::coercion_ctx_listing;
use buck2_interpreter_for_build::attrs::coerce::testing::to_value;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::coerced_deps_collector::CoercedDepsCollector;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use buck2_node::attrs::configured_info::ConfiguredAttrInfo;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::fmt_context::AttrFmtContext;
use buck2_node::attrs::testing::configuration_ctx;
use gazebo::prelude::*;
use indoc::indoc;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Module;
use starlark::values::Heap;
use starlark::values::Value;

use crate::attrs::resolve::configured_attr::ConfiguredAttrExt;
use crate::attrs::resolve::testing::resolution_ctx;
use crate::attrs::resolve::testing::resolution_ctx_with_providers;
use crate::interpreter::rule_defs::cmd_args::DefaultCommandLineContext;
use crate::interpreter::rule_defs::cmd_args::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::provider::registration::register_builtin_providers;

#[test]
fn test() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    // Check that `x` is captured with the function
    let value = to_value(
        &env,
        &globals,
        indoc!(
            r#"
                [[
                    ["hello", "world!"]
                    + select({
                        "//some:config": ["some"],
                        "DEFAULT": ["okay"] + select({
                            "//other:config": ["other"],
                            "DEFAULT": ["default", "for", "realz"],
                        }),
                    })
                    + ["..."]
                    + ["..."]
                ]]
                "#
        ),
    );

    let attr = AttrType::list(AttrType::list(AttrType::list(AttrType::string())));

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "[[[\"hello\",\"world!\"]+select(\"root//some:config\"=[\"some\"],\"DEFAULT\"=[\"okay\"]+select(\"root//other:config\"=[\"other\"],\"DEFAULT\"=[\"default\",\"for\",\"realz\"]))+[\"...\"]+[\"...\"]]]",
        coerced.as_display_no_ctx().to_string()
    );

    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[[[\"hello\",\"world!\",\"okay\",\"other\",\"...\",\"...\"]]]",
        configured.as_display_no_ctx().to_string()
    );

    let ctx = resolution_ctx(&env);
    let resolved = configured.resolve_single(&PackageLabel::testing(), &ctx)?;
    assert_eq!(
        "[[[\"hello\", \"world!\", \"okay\", \"other\", \"...\", \"...\"]]]",
        resolved.to_string()
    );

    Ok(())
}

#[test]
fn test_string() -> anyhow::Result<()> {
    let env = Module::new();
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();
    let attr = AttrType::string();
    let value = to_value(&env, &globals, r#""a" + select({"DEFAULT": "b"})"#);

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(r#""ab""#, configured.as_display_no_ctx().to_string());

    Ok(())
}

#[test]
fn test_invalid_concat_coercion_into_one_of() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    let value = to_value(
        &env,
        &globals,
        indoc!(
            r#"
            [True] + select({"DEFAULT": ["foo"]})
            "#
        ),
    );
    let attr = AttrType::one_of(vec![
        AttrType::list(AttrType::bool()),
        AttrType::list(AttrType::string()),
    ]);

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    coerced
        .configure(&configuration_ctx())
        .expect_err("Should fail to concatenate configured lists");
    Ok(())
}

#[test]
fn test_any() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc(vec!["//some:target", "cell1//named:target[foo]"]);
    let attr = AttrType::any();

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "[\"//some:target\",\"cell1//named:target[foo]\"]",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[\"//some:target\",\"cell1//named:target[foo]\"]",
        configured.as_display_no_ctx().to_string()
    );

    let value = Value::new_none();
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!("None", coerced.as_display_no_ctx().to_string());
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!("None", configured.as_display_no_ctx().to_string());

    let value = Value::new_bool(true);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!("True", coerced.as_display_no_ctx().to_string());
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!("True", configured.as_display_no_ctx().to_string());

    let value = Value::new_int(42);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!("42", coerced.as_display_no_ctx().to_string());
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!("42", configured.as_display_no_ctx().to_string());

    Ok(())
}

#[test]
fn test_option() -> anyhow::Result<()> {
    let heap = Heap::new();
    let attr = AttrType::option(AttrType::list(AttrType::string()));

    let value = heap.alloc(vec!["string1", "string2"]);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "[\"string1\",\"string2\"]",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[\"string1\",\"string2\"]",
        configured.as_display_no_ctx().to_string()
    );

    let value = Value::new_none();
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!("None", coerced.as_display_no_ctx().to_string());
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!("None", configured.as_display_no_ctx().to_string());

    Ok(())
}

#[test]
fn test_dict() -> anyhow::Result<()> {
    let env = Module::new();
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();
    let value = to_value(&env, &globals, r#"{"b":["1"],"a":[]}"#);

    let attr = AttrType::dict(AttrType::string(), AttrType::list(AttrType::string()), true);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "{\"a\": [],\"b\": [\"1\"]}",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "{\"a\": [],\"b\": [\"1\"]}",
        configured.as_display_no_ctx().to_string()
    );

    let attr = AttrType::dict(
        AttrType::string(),
        AttrType::list(AttrType::string()),
        false,
    );
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "{\"b\": [\"1\"],\"a\": []}",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "{\"b\": [\"1\"],\"a\": []}",
        configured.as_display_no_ctx().to_string()
    );

    let value = to_value(
        &env,
        &globals,
        r#"{"b":["1"],"a":[]} + select({"DEFAULT": { "c": []}})"#,
    );
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        r#"{"b": ["1"],"a": [],"c": []}"#,
        configured.as_display_no_ctx().to_string()
    );

    Ok(())
}

#[test]
fn test_one_of() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc("one");
    let values = heap.alloc(vec!["test", "extra"]);

    let attr = AttrType::one_of(vec![AttrType::string(), AttrType::list(AttrType::string())]);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!("\"one\"", coerced.as_display_no_ctx().to_string());
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!("\"one\"", configured.as_display_no_ctx().to_string());

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), values)?;
    assert_eq!(
        "[\"test\",\"extra\"]",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[\"test\",\"extra\"]",
        configured.as_display_no_ctx().to_string()
    );

    let attr = AttrType::one_of(Vec::new());
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value);
    assert!(coerced.is_err());

    Ok(())
}

#[test]
fn test_label() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc(vec!["//some:target", "cell1//named:target[foo]"]);

    let attr = AttrType::list(AttrType::dep(Vec::new()));

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "[\"root//some:target\",\"cell1//named:target[foo]\"]",
        coerced.as_display_no_ctx().to_string()
    );

    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[\"root//some:target (<testing>)\",\"cell1//named:target[foo] (<testing>)\"]",
        configured.as_display_no_ctx().to_string()
    );

    Ok(())
}

#[test]
fn test_coerced_deps() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    let content = indoc!(
        r#"
            ["//some:target", "cell1//named:target[foo]"] + select({
                "//some:config": ["cell1//named:target[bar]"],
                "DEFAULT": ["cell1//:okay"] + select({
                    "cell1//other:config": ["//some:target2"],
                    "DEFAULT": ["//:default1", "//:default2"],
                }),
            }) + ["//:other"]
            "#
    );
    // Check that `x` is captured with the function
    let value = to_value(&env, &globals, content);

    let attr = AttrType::list(AttrType::dep(Vec::new()));
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;

    let mut visitor = CoercedDepsCollector::new();
    coerced.traverse(&PackageLabel::testing(), &mut visitor)?;
    let CoercedDepsCollector {
        deps,
        configuration_deps,
        ..
    } = visitor;
    let deps: Vec<_> = deps.iter().map(|t| t.to_string()).collect();
    let config_deps: Vec<_> = configuration_deps.iter().map(|t| t.to_string()).collect();

    let expected_deps = vec![
        "root//some:target",
        "cell1//named:target",
        "cell1//:okay",
        "root//some:target2",
        "root//:default1",
        "root//:default2",
        "root//:other",
    ];

    assert_eq!(expected_deps, deps);

    let expected_config_deps = vec!["root//some:config", "cell1//other:config"];
    assert_eq!(expected_config_deps, config_deps);

    Ok(())
}

#[test]
fn test_configured_deps() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    let content = indoc!(
        r#"
            ["//some:target", "cell1//named:target[foo]"] + select({
                "//some:config": ["cell1//named:target[bar]"],
                "DEFAULT": ["cell1//:okay"] + select({
                    "cell1//other:config": ["//some:target2"],
                    "DEFAULT": ["//:default1", "//:default2"],
                }),
            }) + ["//:other"]
            "#
    );
    // Check that `x` is captured with the function
    let value = to_value(&env, &globals, content);

    let attr = AttrType::list(AttrType::dep(Vec::new()));
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured = coerced.configure(&configuration_ctx())?;

    let mut info = ConfiguredAttrInfo::new();
    configured.traverse(&PackageLabel::testing(), &mut info)?;

    let expected_deps = vec![
        "root//some:target",
        "cell1//named:target[foo]",
        "cell1//:okay",
        "root//:default1",
        "root//:default2",
        "root//:other",
    ];

    assert_eq!(
        expected_deps.map(|s| format!("{} (<testing>)", s)),
        info.deps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    // Check also that execution deps are handled slightly differently.
    let attr_exec = AttrType::list(AttrType::exec_dep(Vec::new()));
    let coerced_exec = attr_exec.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured_exec = coerced_exec.configure(&configuration_ctx())?;
    let mut info = ConfiguredAttrInfo::new();
    configured_exec.traverse(&PackageLabel::testing(), &mut info)?;
    eprintln!("{:?}", info);
    assert_eq!(
        expected_deps.map(|s| format!("{} (cfg_for//:testing_exec)", s)),
        info.execution_deps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[test]
fn test_resolved_deps() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .with(crate::interpreter::rule_defs::register_rule_defs)
        .build();

    let env = Module::new();
    let content = indoc!(
        r#"
            ["//sub/dir:foo", "//sub/dir:foo[bar]"]
            "#
    );
    // Check that `x` is captured with the function
    let value = to_value(&env, &globals, content);

    let attr = AttrType::list(AttrType::dep(Vec::new()));
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured = coerced.configure(&configuration_ctx())?;
    let resolution_ctx = resolution_ctx(&env);
    let resolved = configured.resolve_single(&PackageLabel::testing(), &resolution_ctx)?;

    env.set("res", resolved);
    let content = indoc!(
        r#"
            foo = res[0]
            bar = res[1]
            def assert_eq(a, b):
                if a != b:
                    fail("Expected {} == {}".format(a, b))

            assert_eq(foo[DefaultInfo].sub_targets["bar"][DefaultInfo], bar[DefaultInfo])
            assert_eq(
                ("sub/dir", "foo", None),
                (foo.label.package, foo.label.name, foo.label.sub_target)
            )
            assert_eq(
                ("sub/dir", "foo", ["bar"]),
                (bar.label.package, bar.label.name, bar.label.sub_target),
            )
            None
            "#
    );

    let success = to_value(&env, &globals, content);
    assert_eq!(true, success.is_none());
    Ok(())
}

#[test]
fn test_dep_requires_providers() -> anyhow::Result<()> {
    let env = Module::new();
    let (resolution_ctx, provider_ids) = resolution_ctx_with_providers(&env);

    let heap = Heap::new();
    let foo_only = heap.alloc("//sub/dir:foo[foo_only]");

    let attr = AttrType::dep(provider_ids.clone());
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), foo_only)?;
    let configured = coerced.configure(&configuration_ctx())?;

    let err = configured
        .resolve_single(&PackageLabel::testing(), &resolution_ctx)
        .expect_err("Should have failed");
    assert_eq!(
        true,
        err.to_string()
            .contains("required provider `BarInfo` was not found")
    );

    let foo_and_bar = heap.alloc("//sub/dir:foo[foo_and_bar]");

    let attr = AttrType::dep(provider_ids);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), foo_and_bar)?;
    let configured = coerced.configure(&configuration_ctx())?;

    // This dep has both FooInfo and BarInfo, so it should resolve properly
    configured.resolve_single(&PackageLabel::testing(), &resolution_ctx)?;

    Ok(())
}

#[test]
fn test_source_missing() {
    let heap = Heap::new();
    let value = heap.alloc(vec!["foo/bar.cpp"]);
    let attr = AttrType::list(AttrType::source(false));

    // FIXME: T85510500 Enable this test properly once we can error out on missing files
    match attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value) {
        Ok(_) => eprintln!("Todo, turn this into an error once T85510500 is fixed"),
        Err(e) => {
            let s = format!("{:#}", e);
            assert!(
                s.contains("Source file `foo/bar.cpp` does not exist"),
                "Got error {}",
                s
            )
        }
    }
}

#[test]
fn test_source_label() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc(vec![
        "//some:target",
        "cell1//named:target[foo]",
        "foo/bar.cpp",
    ]);

    let attr = AttrType::list(AttrType::source(false));

    let coerced = attr.coerce(
        AttrIsConfigurable::Yes,
        &coercion_ctx_listing(PackageListing::testing_files(&["foo/bar.cpp"])),
        value,
    )?;
    assert_eq!(
        "[\"root//some:target\",\"cell1//named:target[foo]\",\"root//package/subdir/foo/bar.cpp\"]",
        coerced
            .as_display(&AttrFmtContext {
                package: Some(PackageLabel::testing())
            })
            .to_string(),
    );

    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        concat!(
            "[\"root//some:target (<testing>)\",",
            "\"cell1//named:target[foo] (<testing>)\",",
            "\"root//package/subdir/foo/bar.cpp\"]",
        ),
        configured
            .as_display(&AttrFmtContext {
                package: Some(PackageLabel::testing())
            })
            .to_string(),
    );

    Ok(())
}

#[test]
fn test_source_label_deps() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    let content = indoc!(
        r#"
            ["//some:target", "cell1//named:target[foo]", "some/target.cpp"] + select({
                "//some:config": ["cell1//named:target[bar]", "cell1/named/target/bar.cpp"],
                "DEFAULT": ["cell1//:okay", "cell1/okay.cpp"] + select({
                    "cell1//other:config": ["//some:target2", "some/target2.cpp"],
                    "DEFAULT": ["//:default1", "//:default2", "default.cpp"],
                }),
            }) + ["//:other", "other.cpp"]
            "#
    );
    // Check that `x` is captured with the function
    let value = to_value(&env, &globals, content);

    let attr = AttrType::list(AttrType::source(false));
    let coerced = attr.coerce(
        AttrIsConfigurable::Yes,
        &coercion_ctx_listing(PackageListing::testing_files(&[
            "some/target.cpp",
            "cell1/named/target/bar.cpp",
            "cell1/okay.cpp",
            "some/target2.cpp",
            "other.cpp",
            "default.cpp",
        ])),
        value,
    )?;

    let mut visitor = CoercedDepsCollector::new();
    coerced.traverse(&PackageLabel::testing(), &mut visitor)?;
    let CoercedDepsCollector {
        deps,
        configuration_deps,
        ..
    } = visitor;
    let deps: Vec<_> = deps.iter().map(|t| t.to_string()).collect();
    let config_deps: Vec<_> = configuration_deps.iter().map(|t| t.to_string()).collect();

    let expected_deps = vec![
        "root//some:target",
        "cell1//named:target",
        "cell1//:okay",
        "root//some:target2",
        "root//:default1",
        "root//:default2",
        "root//:other",
    ];

    assert_eq!(expected_deps, deps);

    let expected_config_deps = vec!["root//some:config", "cell1//other:config"];
    assert_eq!(expected_config_deps, config_deps);

    Ok(())
}

#[test]
fn test_source_label_resolution() -> anyhow::Result<()> {
    fn resolve_and_test(content: &str, test_content: &str, files: &[&str]) -> anyhow::Result<()> {
        let env = Module::new();

        let globals = GlobalsBuilder::extended()
            .with(buck2_interpreter::build_defs::native_module)
            .with(register_builtin_providers)
            .build();

        let value = to_value(&env, &globals, content);

        let attr = AttrType::list(AttrType::source(false));
        let coerced = attr.coerce(
            AttrIsConfigurable::Yes,
            &coercion_ctx_listing(PackageListing::testing_files(files)),
            value,
        )?;
        let configured = coerced.configure(&configuration_ctx())?;
        let resolution_ctx = resolution_ctx(&env);
        let resolved = configured.resolve_single(&PackageLabel::testing(), &resolution_ctx)?;

        env.set("res", resolved);
        let success = to_value(&env, &globals, test_content);
        assert_eq!(true, success.is_none());
        Ok(())
    }

    let content = indoc!(r#"["//sub/dir:foo", "//sub/dir:foo[multiple]", "baz/quz.cpp"]"#);
    let test_content = indoc!(
        r#"
            def assert_eq(a, b):
                if a != b:
                    fail("Expected {} == {}".format(a, b))

            expected = ["default.cpp", "bar1.cpp", "bar2.cpp", "bar3.cpp", "quz.cpp"]
            names = [f.basename for f in res]
            assert_eq(expected, names)
            None
            "#
    );
    resolve_and_test(content, test_content, &["baz/quz.cpp"])?;

    let content = indoc!(r#"["//sub/dir:foo", "//sub/dir:foo[single]", "baz/quz.cpp"]"#);
    let test_content = indoc!(
        r#"
            def assert_eq(a, b):
                if a != b:
                    fail("Expected {} == {}".format(a, b))

            expected = ["default.cpp", "bar1.cpp", "quz.cpp"]
            names = [f.basename for f in res]
            assert_eq(expected, names)
            None
            "#
    );
    resolve_and_test(content, test_content, &["baz/quz.cpp"])?;

    let content = indoc!(r#"["//sub/dir:foo", "//sub/dir:foo[zero]", "baz/quz.cpp"]"#);
    let test_content = indoc!(
        r#"
            def assert_eq(a, b):
                if a != b:
                    fail("Expected {} == {}".format(a, b))

            expected = ["default.cpp", "quz.cpp"]
            names = [f.basename for f in res]
            assert_eq(expected, names)
            None
            "#
    );
    resolve_and_test(content, test_content, &["baz/quz.cpp"])
}

#[test]
fn test_single_source_label_fails_if_multiple_returned() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc("//sub/dir:foo[multiple]");
    let env = Module::new();

    let attr = AttrType::source(false);
    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    let configured = coerced.configure(&configuration_ctx())?;
    let resolution_ctx = resolution_ctx(&env);
    let err = configured
        .resolve_single(&PackageLabel::testing(), &resolution_ctx)
        .expect_err("Getting multiple values when expecting a single one should fail");

    assert_eq!(true, err.to_string().contains("Expected a single artifact"));
    assert_eq!(true, err.to_string().contains("3 artifacts"));
    Ok(())
}

#[test]
fn test_arg() -> anyhow::Result<()> {
    let heap = Heap::new();
    let value = heap.alloc("$(exe //some:exe) --file=$(location \"//some:location\")");

    let attr = AttrType::arg();

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    // Note that targets are canonicalized.
    assert_eq!(
        "\"$(exe root//some:exe) --file=$(location root//some:location)\"",
        coerced.as_display_no_ctx().to_string()
    );
    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "\"$(exe root//some:exe (cfg_for//:testing_exec)) --file=$(location root//some:location (<testing>))\"",
        configured.as_display_no_ctx().to_string()
    );

    let mut visitor = CoercedDepsCollector::new();
    coerced.traverse(&PackageLabel::testing(), &mut visitor)?;
    let CoercedDepsCollector {
        deps, exec_deps, ..
    } = visitor;
    let deps: Vec<_> = deps.iter().map(|t| t.to_string()).collect();
    let exec_deps: Vec<_> = exec_deps.iter().map(|t| t.to_string()).collect();

    let mut info = ConfiguredAttrInfo::new();
    configured.traverse(&PackageLabel::testing(), &mut info)?;

    let expected_deps = vec!["root//some:location"];
    let expected_exec_deps = vec!["root//some:exe"];
    let expected_configured_deps = vec!["root//some:location (<testing>)"];
    let expected_configured_exec_deps = vec!["root//some:exe (cfg_for//:testing_exec)"];

    assert_eq!(expected_deps, deps);
    assert_eq!(expected_exec_deps, exec_deps);

    assert_eq!(
        expected_configured_deps,
        info.deps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        expected_configured_exec_deps,
        info.execution_deps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[test]
fn test_bool() -> anyhow::Result<()> {
    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .build();

    let env = Module::new();
    let value = to_value(
        &env,
        &globals,
        indoc!(
            r#"
                (
                    [True, False]
                    + select({
                        "//some:config": [True],
                        "DEFAULT": [False],
                    })
                    + [True]
                )
                "#
        ),
    );

    let attr = AttrType::list(AttrType::bool());

    let coerced = attr.coerce(AttrIsConfigurable::Yes, &coercion_ctx(), value)?;
    assert_eq!(
        "[True,False]+select(\"root//some:config\"=[True],\"DEFAULT\"=[False])+[True]",
        coerced.as_display_no_ctx().to_string()
    );

    let configured = coerced.configure(&configuration_ctx())?;
    assert_eq!(
        "[True,False,False,True]",
        configured.as_display_no_ctx().to_string()
    );

    let ctx = resolution_ctx(&env);
    let resolved = configured.resolve_single(&PackageLabel::testing(), &ctx)?;
    assert_eq!("[True, False, False, True]", resolved.to_string());

    Ok(())
}

#[test]
fn test_user_placeholders() -> anyhow::Result<()> {
    let env = Module::new();

    let globals = GlobalsBuilder::extended()
        .with(buck2_interpreter::build_defs::native_module)
        .with(register_builtin_providers)
        .build();

    let resolve = move |value: &str| {
        let attr = AttrType::arg();
        let coerced = attr.coerce(
            AttrIsConfigurable::Yes,
            &coercion_ctx(),
            to_value(&env, &globals, value),
        )?;
        let configured = coerced.configure(&configuration_ctx())?;
        let resolution_ctx = resolution_ctx(&env);
        configured
            .resolve_single(&PackageLabel::testing(), &resolution_ctx)
            .map(|v| {
                // TODO: this is way too unnecessarily verbose for a test.
                let project_fs = ProjectRoot::new(
                    AbsNormPathBuf::try_from(std::env::current_dir().unwrap()).unwrap(),
                );
                let fs = ArtifactFs::new(
                    BuckPathResolver::new(CellResolver::of_names_and_paths(&[(
                        CellName::unchecked_new("cell".into()),
                        CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new(
                            "cell_path".into(),
                        )),
                    )])),
                    BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(
                        "buck_out/v2".into(),
                    )),
                    project_fs,
                );
                let executor_fs = ExecutorFs::new(&fs, PathSeparatorKind::Unix);

                let mut cli = Vec::<String>::new();
                let mut ctx = DefaultCommandLineContext::new(&executor_fs);
                v.as_command_line()
                    .unwrap()
                    .add_to_command_line(&mut cli, &mut ctx)
                    .unwrap();
                cli.join(" ")
            })
    };

    assert_eq!("clang++", resolve(r#""$(CXX)""#)?);
    assert_eq!(
        "hello",
        resolve(r#""$(user_key //sub/dir:keyed_placeholder)""#)?
    );
    assert_eq!(
        "world",
        resolve(r#""$(key_with_args //sub/dir:keyed_placeholder)""#)?
    );
    assert_eq!(
        "big world",
        resolve(r#""$(key_with_args //sub/dir:keyed_placeholder big)""#)?
    );

    let value = r#""$(CXXabcdef)""#;
    match resolve(value) {
        Ok(..) => panic!("expected error resolving {}", value),
        Err(e) => {
            let expected = "no mapping for CXXabcdef";
            let message = format!("{:?}", e);
            assert!(
                message.contains(expected),
                "expected `{}` to contain `{}`",
                message,
                expected
            );
        }
    }

    let value = r#""$(missing_user_key //sub/dir:keyed_placeholder)""#;
    match resolve(value) {
        Ok(..) => panic!("expected error resolving {}", value),
        Err(e) => {
            let expected = "no mapping for missing_user_key";
            let message = format!("{:?}", e);
            assert!(
                message.contains(expected),
                "expected `{}` to contain `{}`",
                message,
                expected
            );
        }
    }

    Ok(())
}
