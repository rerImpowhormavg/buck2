# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under both the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree and the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree.

# TODO(cjhopman): This was generated by scripts/hacks/rules_shim_with_docs.py,
# but should be manually editted going forward. There may be some errors in
# the generated docs, and so those should be verified to be accurate and
# well-formatted (and then delete this TODO)

load(":common.bzl", "prelude_rule")

NdkCxxRuntime = ["system", "gabixx", "stlport", "gnustl", "libcxx"]

legacy_toolchain = prelude_rule(
    name = "legacy_toolchain",
    docs = "",
    examples = None,
    further = None,
    attrs = (
        # @unsorted-dict-items
        {
            "contacts": attrs.list(attrs.string(), default = []),
            "default_host_platform": attrs.option(attrs.configuration_label(), default = None),
            "labels": attrs.list(attrs.string(), default = []),
            "licenses": attrs.list(attrs.source(), default = []),
            "toolchain_name": attrs.string(default = ""),
            "within_view": attrs.option(attrs.option(attrs.list(attrs.string())), default = None),
        }
    ),
)

ndk_toolchain = prelude_rule(
    name = "ndk_toolchain",
    docs = "",
    examples = None,
    further = None,
    attrs = (
        # @unsorted-dict-items
        {
            "contacts": attrs.list(attrs.string(), default = []),
            "cxx_runtime": attrs.option(attrs.enum(NdkCxxRuntime), default = None),
            "cxx_toolchain": attrs.dep(),
            "default_host_platform": attrs.option(attrs.configuration_label(), default = None),
            "labels": attrs.list(attrs.string(), default = []),
            "licenses": attrs.list(attrs.source(), default = []),
            "objdump": attrs.source(),
            "shared_runtime_path": attrs.option(attrs.source(), default = None),
            "strip_apk_libs_flags": attrs.option(attrs.list(attrs.arg()), default = None),
            "within_view": attrs.option(attrs.option(attrs.list(attrs.string())), default = None),
        }
    ),
)

uncategorized_rules = struct(
    legacy_toolchain = legacy_toolchain,
    ndk_toolchain = ndk_toolchain,
)
