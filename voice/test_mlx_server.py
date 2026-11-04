import unittest
from mlx_server import fixed_model_loader


class FixedModelTests(unittest.TestCase):
    def test_alias_loads_only_configured_directory(self):
        seen = []
        loader = fixed_model_loader("/models/voice", lambda path: seen.append(path) or "loaded")
        self.assertEqual(loader("agentrelay"), "loaded")
        self.assertEqual(seen, ["/models/voice"])
        for name in ("../other", "publisher/unrelated", "", "/models/voice"):
            with self.assertRaisesRegex(ValueError, "only serves"):
                loader(name)
        self.assertEqual(seen, ["/models/voice"])


if __name__ == "__main__":
    unittest.main()
