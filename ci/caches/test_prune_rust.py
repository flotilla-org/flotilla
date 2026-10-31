import unittest

from ci.caches.prune_rust import superseded


def cache(key, created, ref="refs/heads/main"):
    return {"key": key, "ref": ref, "created_at": created}


class SupersededTests(unittest.TestCase):
    def test_keeps_only_the_newest_cache_of_each_restore_key(self):
        base = "v0-rust-1.99.0-clippy-Linux-x64-0a1b2c3d"
        caches = [
            cache(f"{base}-11111111", "2026-10-01"),
            cache(f"{base}-22222222", "2026-10-02"),
            cache("v0-rust-1.99.0-test-v2-Linux-x64-0a1b2c3d-33333333", "2026-10-01"),
        ]
        self.assertEqual(superseded(caches), [f"{base}-11111111"])

    def test_drops_an_unhashed_key_once_a_hashed_one_replaces_it(self):
        caches = [
            cache("v0-rust-1.99.0-clippy-Linux-x64", "2026-10-01"),
            cache("v0-rust-1.99.0-clippy-Linux-x64-0a1b2c3d-11111111", "2026-10-02"),
            cache("v0-rust-1.99.0-test-Linux-x64", "2026-10-01"),
            cache("v0-rust-1.99.0-test-v2-Linux-x64-0a1b2c3d-11111111", "2026-10-02"),
        ]
        self.assertEqual(superseded(caches), ["v0-rust-1.99.0-clippy-Linux-x64"])

    def test_leaves_other_refs_and_other_caches_alone(self):
        caches = [
            cache("v0-rust-1.99.0-clippy-Linux-x64-0a1b2c3d-11111111", "2026-10-01", ref="refs/pull/1/merge"),
            cache("v0-rust-1.99.0-clippy-Linux-x64-0a1b2c3d-22222222", "2026-10-02"),
            cache("cleat-environment-linux-aaaa", "2026-10-01"),
            cache("cleat-environment-linux-bbbb", "2026-10-02"),
        ]
        self.assertEqual(superseded(caches), [])


if __name__ == "__main__":
    unittest.main()
