"""Offline, source-level regression for the SDK's network default macros.

Run: python3 tests/network_defaults.py -v
Requires a local SDK (IDF_PATH, or the newest esp-idf-sys CMake cache).
No compiler, network, hardware, generated bindings, or C runtime is used. This
checks initializer expressions, not ABI/layout, cast widths, evaluated Kconfig
values, or driver behaviour; those remain build/integration responsibilities.
"""
import os
import pathlib
import re
import tempfile
import unittest

PROJECT = pathlib.Path(__file__).resolve().parents[1]
CACHE_GLOB = "target/riscv32imafc-esp-espidf/release/build/esp-idf-sys/*/out/build/CMakeCache.txt"
IDENT = r"[A-Za-z_][A-Za-z_0-9]*"
BOOL_FIELDS = {"sta_disconnected_pm", "dump_hesigb_enable", "privacy_enhancements"}
POINTER_FIELDS = {"global_user_ctx", "global_transport_ctx", "if_name"}
CALLBACK_FIELDS = {"global_user_ctx_free_fn", "global_transport_ctx_free_fn",
                   "open_fn", "close_fn", "uri_match_fn"}


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def discover_idf(project=PROJECT, environ=None):
    environ = os.environ if environ is None else environ
    if environ.get("IDF_PATH"):
        return pathlib.Path(environ["IDF_PATH"]).expanduser()
    caches = sorted(project.glob(CACHE_GLOB),
                    key=lambda p: (p.stat().st_mtime_ns, str(p)), reverse=True)
    require(caches, "No SDK found: set IDF_PATH or build esp-idf-sys first")
    # Read only path entries, never print cache contents (which may hold secrets).
    entries = {}
    for line in caches[0].read_text().splitlines():
        match = re.fullmatch(r"(IDF_PATH|esp-idf_SOURCE_DIR)(?::[^=]+)?=(.+)", line)
        if match:
            require(match[1] not in entries, "Duplicate SDK path entry in newest cache")
            entries[match[1]] = match[2]
    path = entries.get("IDF_PATH") or entries.get("esp-idf_SOURCE_DIR")
    require(path, "Newest CMake cache has no SDK path; set IDF_PATH")
    return pathlib.Path(path).expanduser()


def uncomment(source):
    return re.sub(r"/\*.*?\*/|//[^\n]*", "", source, flags=re.S)


def split_top(source, separator):
    """Split only outside balanced delimiters, rejecting malformed input."""
    stack, parts, start = [], [], 0
    pairs = {")": "(", "]": "[", "}": "{"}
    for index, char in enumerate(source):
        if char in "([{":
            stack.append(char)
        elif char in ")]}":
            require(stack and stack.pop() == pairs[char], "Unbalanced initializer")
        elif char == separator and not stack:
            parts.append(source[start:index].strip())
            start = index + 1
    require(not stack, "Unbalanced initializer")
    parts.append(source[start:].strip())
    return parts


def braced(source, start):
    require(source[start] == "{", "Expected initializer brace")
    depth = 0
    for index in range(start, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[start + 1:index], index + 1
    raise AssertionError("Unclosed initializer")


def macro_body(source, name):
    source = uncomment(re.sub(r"\\\r?\n", "", source))
    matches = list(re.finditer(r"^\s*#\s*define\s+" + re.escape(name)
                              + r"\(\)[ \t]*(.*)$", source, re.M))
    require(len(matches) == 1, f"Expected exactly one {name} macro")
    expression = matches[0][1].strip()
    require(expression.startswith("{"), f"Unsupported {name} macro")
    body, end = braced(expression, 0)
    require(not expression[end:].strip(), f"Unexpected tokens after {name}")
    return body


def fields(body, language):
    require(".." not in body, "Rust default/update syntax is forbidden")
    items = split_top(body, ",")
    if items[-1] == "":
        items.pop()
    result = {}
    pattern = (r"\.\s*(" + IDENT + r")\s*=\s*(.+)" if language == "c"
               else r"(" + IDENT + r")\s*:\s*(.+)")
    for item in items:
        match = re.fullmatch(pattern, item, re.S)
        require(match is not None, f"Unsupported {language} field initializer")
        name, expression = match.groups()
        require(name not in result, f"Duplicate field: {name}")
        result[name] = expression.strip()
    require(result, "Empty initializer")
    return result


def rust_fields(source, function, struct):
    source = uncomment(source)
    matches = list(re.finditer(r"\bfn\s+" + function + r"\s*\(\s*\)\s*"
                              + r"->\s*sys::" + struct + r"\s*\{", source))
    require(len(matches) == 1, f"Expected exactly one {function} function")
    body, _ = braced(source, matches[0].end() - 1)
    match = re.match(r"\s*sys::" + struct + r"\s*\{", body)
    require(match is not None, f"Expected direct {struct} initializer")
    initializer, end = braced(body, match.end() - 1)
    require(not body[end:].strip(), f"Unexpected statements in {function}")
    return fields(initializer, "rust")


def unparen(expression):
    while expression.startswith("(") and expression.endswith(")"):
        # The opening parenthesis must surround the entire expression.
        depth, end = 0, None
        for index, char in enumerate(expression):
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    end = index
                    break
        if end != len(expression) - 1:
            break
        expression = expression[1:-1].strip()
    return expression


def expression_tree(expression, language):
    """Deliberately small grammar: unsupported SDK expressions fail closed."""
    expression = unparen(expression.strip())
    for operator in ("|", "+"):
        parts = split_top(expression, operator)
        if len(parts) > 1:
            return (operator, tuple(expression_tree(p, language) for p in parts))
    if re.fullmatch(r"(?:0[xX][0-9a-fA-F]+|[0-9]+)(?:[uUlL]+)?", expression):
        number = re.sub(r"[uUlL]+$", "", expression)
        return ("number", int(number, 16 if number.lower().startswith("0x") else 10))
    if expression in ("true", "false"):
        return ("bool", expression == "true")
    pattern = IDENT if language == "c" else r"sys::(" + IDENT + r")"
    match = re.fullmatch(pattern, expression)
    if match:
        return ("symbol", expression if language == "c" else match[1])
    raise AssertionError(f"Unsupported {language} expression: {expression}")


def normalize(expression, language, field, idle_zero, core_alias_verified=False):
    expression = expression.strip()
    if language == "rust":
        # Cast widths/types are checked by the parent Rust build, not evaluated here.
        expression = re.sub(r"\s+as\s+(?:i32|u8|u16|u64|usize|sys::BaseType_t)\s*$",
                            "", expression)
        if field in BOOL_FIELDS:
            match = re.fullmatch(r"sys::(" + IDENT + r")\s*!=\s*0", expression)
            require(match is not None, f"Expected nonzero-to-bool conversion: {field}")
            return ("symbol", match[1])
        if field == "osi_funcs":
            match = re.fullmatch(r"ptr::addr_of_mut!\(\s*sys::(" + IDENT + r")\s*\)", expression)
            require(match is not None, "Expected addr_of_mut for Wi-Fi OS callbacks")
            return ("address", match[1])
        if field == "wpa_crypto_funcs":
            match = re.fullmatch(r"unsafe\s*\{\s*sys::(" + IDENT + r")\s*\}", expression)
            require(match is not None, "Expected direct Wi-Fi crypto static copy")
            return ("symbol", match[1])
        if field in POINTER_FIELDS:
            require(re.fullmatch(r"ptr::null_mut\(\s*\)", expression),
                    f"Expected null data pointer: {field}")
            return ("null",)
        if field in CALLBACK_FIELDS:
            require(expression == "None", f"Expected null callback: {field}")
            return ("null",)
    else:
        if expression == "NULL":
            require(field in POINTER_FIELDS | CALLBACK_FIELDS, "Unexpected C NULL field")
            return ("null",)
        match = re.fullmatch(r"&\s*(" + IDENT + r")", expression)
        if match:
            return ("address", match[1])
    tree = expression_tree(expression, language)
    if (language == "rust" and field == "core_id"
            and tree == ("symbol", "FRIDGE_TSK_NO_AFFINITY")):
        require(core_alias_verified, "Core affinity alias must be verified in bindings.h")
        return ("symbol", "tskNO_AFFINITY")
    if field == "task_priority" and tree == (
            "+", (("symbol", "tskIDLE_PRIORITY"), ("number", 5))):
        require(idle_zero, "tskIDLE_PRIORITY must be verified as zero in SDK headers")
        return ("number", 5)
    return tree


def verify_core_alias(bindings):
    source = uncomment(bindings)
    alias = "FRIDGE_TSK_NO_AFFINITY"
    require(len(re.findall(r"\b" + alias + r"\b", source)) == 1,
            "Expected exactly one core affinity alias in bindings.h")
    members = []
    for enum in re.finditer(r"\benum\s*\{([^{}]*)\}\s*;", source, re.S):
        members.extend(member for member in split_top(enum[1], ",")
                       if re.search(r"\b" + alias + r"\b", member))
    require(len(members) == 1 and re.fullmatch(
        alias + r"\s*=\s*tskNO_AFFINITY", members[0]),
        "Core affinity alias must map exactly to tskNO_AFFINITY in a header enum")
    return True


def check_defaults(rust, header, macro, function, struct, idle_zero=False, bindings=None):
    core_alias_verified = verify_core_alias(bindings) if bindings is not None else False
    c_fields = fields(macro_body(header, macro), "c")
    r_fields = rust_fields(rust, function, struct)
    require(c_fields.keys() == r_fields.keys(),
            f"{macro} fields differ: SDK-only={sorted(c_fields.keys() - r_fields.keys())}, "
            f"Rust-only={sorted(r_fields.keys() - c_fields.keys())}")
    for field in c_fields:
        c = normalize(c_fields[field], "c", field, idle_zero)
        r = normalize(r_fields[field], "rust", field, idle_zero, core_alias_verified)
        require(c == r, f"{macro}.{field} differs: SDK={c!r}, Rust={r!r}")


def verify_idle_zero(idf):
    # IDF currently defines this in task.h, not FreeRTOS.h. Inspect both kernel
    # variants so an unselected/ambiguous changed definition cannot pass silently.
    definitions = []
    for header in sorted((idf / "components/freertos").rglob("*.h")):
        text = uncomment(re.sub(r"\\\r?\n", "", header.read_text()))
        definitions.extend(re.findall(r"^\s*#\s*define\s+tskIDLE_PRIORITY\b([^\n]*)",
                                      text, re.M))
    require(definitions, "Cannot find SDK tskIDLE_PRIORITY definition")
    for definition in definitions:
        value = unparen(definition.strip())
        value = re.sub(r"^\(\s*UBaseType_t\s*\)\s*", "", value)
        require(expression_tree(value, "c") == ("number", 0),
                "SDK tskIDLE_PRIORITY is not provably zero")
    return True


def check_guard(rust, version):
    source = uncomment(rust)
    guards = re.findall(r"const\s+_\s*:\s*\(\s*\)\s*=\s*assert!\s*\(\s*"
                       r"sys::ESP_IDF_VERSION_MAJOR\s*==\s*(\d+)\s*&&\s*"
                       r"sys::ESP_IDF_VERSION_MINOR\s*==\s*(\d+)\s*,\s*"
                       r'"[^"\\]*"\s*\)\s*;', source)
    require(guards == [("6", "1")], "Expected unconditional compile-time IDF 6.1 guard")
    # A cfg attribute could disable the assertion; reject attributes on this module.
    require("#[" not in source and "#![" not in source, "Conditional/attributed defaults need review")
    for name, expected in (("MAJOR", "6"), ("MINOR", "1")):
        definitions = re.findall(r"^\s*#\s*define\s+ESP_IDF_VERSION_" + name
                                 + r"\s+([^\n]+)", uncomment(version), re.M)
        require(definitions == [expected], "Actual SDK is not ESP-IDF 6.1")


class NetworkDefaultsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        idf = discover_idf()
        cls.rust = (PROJECT / "src/wifi/defaults.rs").read_text()
        cls.bindings = (PROJECT / "idf/bindings.h").read_text()
        cls.wifi = (idf / "components/esp_wifi/include/esp_wifi.h").read_text()
        cls.http = (idf / "components/esp_http_server/include/esp_http_server.h").read_text()
        cls.version = (idf / "components/esp_common/include/esp_idf_version.h").read_text()
        cls.idle_zero = verify_idle_zero(idf)

    def check_wifi(self, header=None, rust=None):
        check_defaults(self.rust if rust is None else rust,
                       self.wifi if header is None else header,
                       "WIFI_INIT_CONFIG_DEFAULT", "wifi_config", "wifi_init_config_t")

    def check_http(self, header=None, rust=None, idle_zero=None, bindings=None):
        check_defaults(self.rust if rust is None else rust,
                       self.http if header is None else header,
                       "HTTPD_DEFAULT_CONFIG", "http_config", "httpd_config_t",
                       self.idle_zero if idle_zero is None else idle_zero,
                       self.bindings if bindings is None else bindings)

    def changed_macro(self, header, macro, old, new):
        body = macro_body(header, macro)
        changed, count = re.subn(old, new, body)
        self.assertEqual(count, 1, "Mutation must change exactly one SDK initializer")
        return f"#define {macro}() {{ {changed} }}\n"

    def test_wifi_matches_actual_sdk(self):
        self.check_wifi()

    def test_http_matches_actual_sdk(self):
        self.check_http()

    def test_version_guard_matches_actual_sdk(self):
        check_guard(self.rust, self.version)

    def test_changed_sdk_numeric_default_fails(self):
        changed = self.changed_macro(self.http, "HTTPD_DEFAULT_CONFIG",
                                     r"(\.stack_size\s*=\s*)4096\b", r"\g<1>4097")
        with self.assertRaisesRegex(AssertionError, "stack_size differs"):
            self.check_http(changed)

    def test_added_sdk_field_fails(self):
        body = macro_body(self.wifi, "WIFI_INIT_CONFIG_DEFAULT")
        changed = f"#define WIFI_INIT_CONFIG_DEFAULT() {{ {body}, .new_sdk_field = 1 }}\n"
        with self.assertRaisesRegex(AssertionError, "SDK-only=.*new_sdk_field"):
            self.check_wifi(changed)

    def test_changed_sdk_wifi_symbol_fails(self):
        changed = self.changed_macro(self.wifi, "WIFI_INIT_CONFIG_DEFAULT",
                                     r"\bWIFI_STATIC_TX_BUFFER_NUM\b", "WIFI_DYNAMIC_TX_BUFFER_NUM")
        with self.assertRaisesRegex(AssertionError, "static_tx_buf_num differs"):
            self.check_wifi(changed)

    def test_missing_rust_field_fails(self):
        changed = re.sub(r"^\s*magic:.*\n", "", self.rust, flags=re.M)
        with self.assertRaisesRegex(AssertionError, "fields differ"):
            self.check_wifi(rust=changed)

    def test_duplicate_sdk_field_fails(self):
        body = macro_body(self.http, "HTTPD_DEFAULT_CONFIG")
        with self.assertRaisesRegex(AssertionError, "Duplicate field"):
            self.check_http(f"#define HTTPD_DEFAULT_CONFIG() {{ {body}, .stack_size = 4096 }}")

    def test_default_update_forbidden(self):
        changed = self.rust.replace("magic: sys::WIFI_INIT_CONFIG_MAGIC as i32,",
                                    "magic: sys::WIFI_INIT_CONFIG_MAGIC as i32,\n..Default::default()")
        with self.assertRaisesRegex(AssertionError, "default/update"):
            self.check_wifi(rust=changed)

    def test_unknown_sdk_expression_fails_closed(self):
        changed = self.changed_macro(self.http, "HTTPD_DEFAULT_CONFIG",
                                     r"(\.stack_size\s*=\s*)4096\b", r"\g<1>new_default()")
        with self.assertRaisesRegex(AssertionError, "Unsupported c expression"):
            self.check_http(changed)

    def test_crypto_static_and_os_pointer_are_checked(self):
        for old, new in (("sys::g_wifi_default_wpa_crypto_funcs", "sys::wrong_crypto"),
                         ("sys::g_wifi_osi_funcs", "sys::wrong_osi")):
            with self.subTest(symbol=old), self.assertRaisesRegex(AssertionError, "differs"):
                self.check_wifi(rust=self.rust.replace(old, new))

    def test_bool_conversion_is_checked(self):
        with self.assertRaisesRegex(AssertionError, "nonzero-to-bool"):
            self.check_wifi(rust=self.rust.replace("sys::WIFI_STA_DISCONNECTED_PM_ENABLED != 0",
                                                 "sys::WIFI_STA_DISCONNECTED_PM_ENABLED == 0"))

    def test_pointer_and_callback_kinds_are_checked(self):
        for old, new in (("global_user_ctx: ptr::null_mut()", "global_user_ctx: None"),
                         ("open_fn: None", "open_fn: ptr::null_mut()")):
            with self.subTest(field=old), self.assertRaisesRegex(AssertionError, "Expected null"):
                self.check_http(rust=self.rust.replace(old, new))

    def test_core_affinity_must_reference_sdk_symbol(self):
        for replacement in ("0", "sys::UNVERIFIED_NO_AFFINITY as sys::BaseType_t"):
            with self.subTest(value=replacement), self.assertRaisesRegex(AssertionError, "core_id differs"):
                self.check_http(rust=self.rust.replace(
                    "sys::FRIDGE_TSK_NO_AFFINITY as sys::BaseType_t", replacement))

    def test_changed_core_alias_mapping_fails(self):
        for value in ("0", "tskNO_AFFINITY + 1", "another_affinity_symbol"):
            changed, count = re.subn(r"(FRIDGE_TSK_NO_AFFINITY\s*=\s*)tskNO_AFFINITY\b",
                                     r"\g<1>" + value, self.bindings)
            self.assertEqual(count, 1, "Mutation must change exactly one alias mapping")
            with self.subTest(value=value), self.assertRaisesRegex(AssertionError, "must map exactly"):
                self.check_http(bindings=changed)

    def test_core_alias_requires_unique_header_enum(self):
        for changed in (self.bindings.replace("FRIDGE_TSK_NO_AFFINITY", "REMOVED_ALIAS"),
                        self.bindings + "\nenum { FRIDGE_TSK_NO_AFFINITY = tskNO_AFFINITY };\n",
                        "#define FRIDGE_TSK_NO_AFFINITY tskNO_AFFINITY\n"):
            with self.subTest(), self.assertRaisesRegex(AssertionError, "core affinity alias|Core affinity alias"):
                self.check_http(bindings=changed)

    def test_core_alias_not_accepted_without_header_verification(self):
        with self.assertRaisesRegex(AssertionError, "alias must be verified"):
            check_defaults(self.rust, self.http, "HTTPD_DEFAULT_CONFIG", "http_config",
                           "httpd_config_t", self.idle_zero)

    def test_priority_requires_verified_idle_zero(self):
        with self.assertRaisesRegex(AssertionError, "must be verified"):
            self.check_http(idle_zero=False)

    def test_changed_idle_header_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            headers = root / "components/freertos"
            headers.mkdir(parents=True)
            (headers / "task.h").write_text("#define tskIDLE_PRIORITY ((UBaseType_t) 1U)\n")
            with self.assertRaisesRegex(AssertionError, "not provably zero"):
                verify_idle_zero(root)

    def test_removed_or_changed_version_guard_fails(self):
        for source in (self.rust.replace("ESP_IDF_VERSION_MINOR == 1", "ESP_IDF_VERSION_MINOR == 2"),
                       self.rust.replace("const _: () = assert!", "// const _: () = assert!"),
                       "#[cfg(any())]\n" + self.rust):
            with self.subTest(), self.assertRaises(AssertionError):
                check_guard(source, self.version)

    def test_discovery_prefers_environment_without_reading_cache(self):
        self.assertEqual(discover_idf(PROJECT, {"IDF_PATH": "/sdk/from/environment"}),
                         pathlib.Path("/sdk/from/environment"))

    def test_discovery_uses_newest_cache_and_source_dir_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for index, contents in enumerate(("IDF_PATH:PATH=/old/sdk\n",
                                              "esp-idf_SOURCE_DIR:STATIC=/new/sdk\nsecret=do-not-log\n")):
                cache = root / CACHE_GLOB.replace("*", str(index))
                cache.parent.mkdir(parents=True)
                cache.write_text(contents)
                os.utime(cache, ns=(index + 1, index + 1))
            self.assertEqual(discover_idf(root, {}), pathlib.Path("/new/sdk"))
            cache.write_text("IDF_PATH:PATH=/preferred/sdk\nesp-idf_SOURCE_DIR:STATIC=/new/sdk\n")
            self.assertEqual(discover_idf(root, {}), pathlib.Path("/preferred/sdk"))
            cache.write_text("secret=do-not-log\n")
            with self.assertRaisesRegex(AssertionError, "Newest CMake cache has no SDK path"):
                discover_idf(root, {})


if __name__ == "__main__":
    unittest.main()
