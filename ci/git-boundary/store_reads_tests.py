"""Store-read guard rail contract, imported by test_check.py so the existing
`unittest discover -p test_check.py` CI command runs it."""
import unittest

import store_reads


def fn(body, signature="fn f(backend: &ResourceBackend, namespaces: Vec<String>, items: Vec<Item>)"):
    return f"{signature} {{ {body} }}"


def lines(source, path="crates/a/src/lib.rs", production=False):
    return [line for line, _, _ in store_reads.violations(source, path, production)]


# Every whole-kind list form the rule recognizes.
LIST_FORMS = [
    "backend.using::<Convoy>(\"ns\").list().await",
    "backend.clone().including_replicas::<Convoy>(&ns).list().await",
    "backend.definitions::<Project>(ns).list().await",
    "self.convoys.list().await",
    "store.list_raw(kind, \"ns\").await",
    "store.list_replicas_raw(kind, \"ns\").await",
    "store.list_including_replicas_typed::<Convoy>(\"ns\").await",
    "resolver.list_replica_sources().await",
    "source.list_and_watch().await",
    "flotilla_store::list_resource_kind(backend, \"ns\", kind).await",
]
# Each construct re-runs its body per iteration or per item.
REPEATING = [
    "for item in items {{ {read}; }}",
    "for item in items {{ if item.ready {{ let x = {read}; }} }}",
    "while ready() {{ {read}; }}",
    "while {read}.is_ok() {{ }}",
    "loop {{ {read}; break; }}",
    "items.iter().map(|item| {read});",
    "futures::stream::iter(items).then(|item| async move {{ {read} }});",
    "items.into_iter().for_each(|item| {{ {read}; }});",
    "items.iter().filter_map(|item| Some({read}));",
    "for item in items {{ tokio::spawn(async move {{ {read}; }}); }}",
]


class StoreReadTests(unittest.TestCase):
    # A whole-kind list in any loop body, loop condition or per-item closure is
    # rejected, in every recognized list form, at the list's line.
    def test_repeated_whole_kind_lists_are_rejected(self):
        for read in LIST_FORMS:
            for shape in REPEATING:
                with self.subTest(read=read, shape=shape):
                    self.assertEqual(lines(fn("\n" + shape.format(read=read))), [2])

    # The same reads run once per call outside repetition: before a loop, in a
    # `for` iterator expression, in a nested fn item, or in a non-per-item closure.
    def test_single_reads_are_allowed(self):
        for read in LIST_FORMS:
            for shape in ["let all = {read};", "for item in {read}?.items {{ use_it(item); }}",
                          "for item in items {{ fn helper(b: &ResourceBackend) {{ {read}; }} }}",
                          "let f = || {read}; let g = result.unwrap_or_else(|e| {read});",
                          "// for item in items {{ {read}; }}", "let s = \"for x in y {{ {read} }}\";"]:
                with self.subTest(read=read, shape=shape):
                    self.assertEqual(lines(fn(shape.format(read=read))), [])

    # Only zero-argument `.list()` on a resolver-shaped or unknown receiver is a
    # store list; locals bound to other values (environment providers) are not.
    def test_list_receivers(self):
        for body in ["for item in items { provider.list(filter).await; }",
                     "for item in items { let (_, provider) = registry.select(kind, None)?; provider.list().await; }",
                     "for item in items { let provider = registry.select(kind)?; provider.list().await; }",
                     "for item in items { gh.issues().list().await; }",
                     "for item in items { let r = backend.using::<Convoy>(\"ns\").get(\"a\"); r.list_names(names).await; }"]:
            with self.subTest(body=body):
                self.assertEqual(lines(fn(body)), [])
        for body in ["for item in items { let r = backend.using::<Convoy>(\"ns\"); r.list().await; }",
                     "for item in items { let mut r = backend.including_replicas::<Convoy>(\"ns\").clone(); r.list().await; }",
                     "for item in items { resolver.list().await; }",
                     "for item in items { let r = backend.using::<C>(\"ns\"); let r = r.clone(); r.list().await; }"]:
            with self.subTest(body=body):
                self.assertEqual(lines(fn(body)), [1])

    # A partition sweep (the loop's own binding directly selects the namespace
    # or store) lists each partition once; deriving the selector from a field of
    # the item, a `while let` binding or an inner loop still repeats reads.
    def test_partition_sweeps(self):
        for body in ["for namespace in namespaces { backend.using::<C>(&namespace).list().await; }",
                     "for ns in namespaces { let r = backend.using::<C>(ns.as_str()); r.list().await; }",
                     "for (_, source) in sources { source.list().await; }",
                     "for ns in namespaces { store.list_raw(kind, &ns).await; }",
                     "namespaces.iter().map(|ns| backend.definitions::<P>(ns).list());"]:
            with self.subTest(body=body):
                self.assertEqual(lines(fn(body)), [])
        for body in ["for item in items { backend.using::<C>(&item.namespace).list().await; }",
                     "while let Some(ns) = next() { backend.using::<C>(&ns).list().await; }",
                     "for ns in namespaces { for item in items { backend.using::<C>(&ns).list().await; } }",
                     "loop { for ns in namespaces { backend.using::<C>(&ns).list().await; } }"]:
            with self.subTest(body=body):
                self.assertEqual(lines(fn(body)), [1])

    # A OnceCell initializer declared outside the loop runs at most once; the
    # same initializer on a cell created per iteration still repeats.
    def test_memoized_reads(self):
        read = "cell.get_or_try_init(|| backend.using::<C>(\"ns\").list()).await"
        self.assertEqual(lines(fn(f"let cell = OnceCell::new();\nfor item in items {{ {read}; }}")), [])
        self.assertEqual(lines(fn(f"for item in items {{ let cell = OnceCell::new(); {read}; }}")), [1])
        self.assertEqual(lines(fn(f"let cell = OnceCell::new(); for item in items {{ cell.get_or_init(|| async {{ for x in y {{ backend.using::<C>(\"ns\").list().await; }} }}).await; }}")), [1])

    # Test code is exempt through the shared traversal; production inclusions of
    # fixtures, and cfg alternatives with a production branch, stay checked.
    def test_test_context(self):
        body = fn("for item in items { backend.using::<C>(\"ns\").list().await; }")
        self.assertEqual(lines("#[cfg(test)] mod tests { " + body + " }"), [])
        self.assertEqual(lines("#[cfg(any(test, unix))] mod tests { " + body + " }"), [1])
        for path in ["crates/a/tests/fixture.rs", "crates/a-testkit/src/lib.rs", "build.rs"]:
            self.assertEqual(lines(body, path), [])
        self.assertEqual(lines(body, "crates/a/tests/fixture.rs", production=True), [1])
        sources = {"src/lib.rs": "#[cfg(test)] mod fixture;", "src/fixture.rs": body}
        self.assertNotIn("src/fixture.rs", store_reads.check.scan_sources(sources, store_reads.violations))

    # Allowlist entries name a function, an exact count and a reason; new
    # findings, changed counts and stale entries all fail.
    def test_allowlist(self):
        body = fn("for item in items { backend.using::<C>(\"ns\").list().await; }")
        sources = {"src/a.rs": body}
        entry = '[[allow]]\npath = "src/a.rs"\nfunction = "f"\nreason = "startup only"\n'
        self.assertEqual(store_reads.scan(sources, entry), [])
        self.assertEqual(len(store_reads.scan(sources, "")), 1)
        self.assertIn("allows 2", store_reads.scan(sources, entry + "count = 2\n")[0])
        self.assertIn("0 found", store_reads.scan({"src/a.rs": "fn f() {}"}, entry)[0])
        self.assertIn("src/b.rs:1", store_reads.scan({**sources, "src/b.rs": body}, entry)[0])
        for broken in [entry.replace("startup only", " "), entry + entry, entry + "count = 0\n",
                       entry + "line = 3\n", '[[allow]]\npath = "src/a.rs"\nreason = "x"\n']:
            with self.subTest(broken=broken), self.assertRaises(ValueError):
                store_reads.load_allowlist(broken)

    # The checked-in allowlist parses and every entry carries a reason.
    def test_repository_allowlist(self):
        allowed = store_reads.load_allowlist(store_reads.ALLOWLIST.read_text())
        self.assertTrue(allowed)
        self.assertTrue(all(path.startswith("crates/") for path, _ in allowed))


if __name__ == "__main__":
    unittest.main()
