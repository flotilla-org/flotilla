#!/usr/bin/env python3
"""Contract tests for dump2json.py: TLC's printed values decode to Quint's ITF encoding."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dump2json import parse_value, states  # noqa: E402

UNIT = {"#tup": []}


class ValueEncoding(unittest.TestCase):
    def test_scalars(self):
        self.assertEqual(parse_value("TRUE"), True)
        self.assertEqual(parse_value("FALSE"), False)
        self.assertEqual(parse_value("-3"), {"#bigint": "-3"})
        self.assertEqual(parse_value('"a \\"quoted\\" word"'), 'a "quoted" word')

    def test_variants_and_unit_payloads(self):
        # Quint compiles a sum value to a one-field record keyed by its constructor.
        self.assertEqual(parse_value('[Requested |-> [tag |-> "UNIT"]]'), {"tag": "Requested", "value": UNIT})
        self.assertEqual(parse_value('[GaveUp |-> "failure"]'), {"tag": "GaveUp", "value": "failure"})
        nested = parse_value('[Refused |-> [MissingEvidence |-> [tag |-> "UNIT"]]]')
        self.assertEqual(nested, {"tag": "Refused", "value": {"tag": "MissingEvidence", "value": UNIT}})

    def test_records_keep_single_lower_case_fields(self):
        # A one-field record with a lower camel case field is a record, not a variant.
        self.assertEqual(parse_value("[absent |-> TRUE]"), {"absent": True})
        self.assertEqual(parse_value("[a |-> 1, b |-> <<>>]"), {"a": {"#bigint": "1"}, "b": []})

    def test_collections(self):
        self.assertEqual(parse_value("{}"), {"#set": []})
        self.assertEqual(parse_value("{1, 2}"), {"#set": [{"#bigint": "1"}, {"#bigint": "2"}]})
        self.assertEqual(parse_value("<<1, 1>>"), [{"#bigint": "1"}, {"#bigint": "1"}])
        self.assertEqual(parse_value("(0 :> TRUE @@ 1 :> FALSE)"), {"#map": [[{"#bigint": "0"}, True], [{"#bigint": "1"}, False]]})
        self.assertEqual(parse_value("(0 :> [Pending |-> [tag |-> \"UNIT\"]])"), {"#map": [[{"#bigint": "0"}, {"tag": "Pending", "value": UNIT}]]})

    def test_malformed_values_are_errors(self):
        for text in ("[a |-> ]", "{1, 2", "1 2", "?"):
            with self.subTest(text=text), self.assertRaises((ValueError, IndexError, TypeError)):
                parse_value(text)

    def test_states_split_on_headers_and_strip_prefixes(self):
        dump = 'State 1:\n/\\ m_state = [Live |-> [Detached |-> [tag |-> "UNIT"]]]\n/\\ m_probe = [Started |-> [tag |-> "UNIT"]]\n\n' \
               'State 2:\n/\\ m_state = [Ended |-> [tag |-> "UNIT"]]\n/\\ m_probe = [ Stepped |->\n      [ from |-> [Ended |-> [tag |-> "UNIT"]] ] ]\n'
        nodes = list(states(dump, prefix="m_"))
        self.assertEqual(len(nodes), 2)
        self.assertEqual(nodes[0]["state"], {"tag": "Live", "value": {"tag": "Detached", "value": UNIT}})
        self.assertEqual(nodes[1]["probe"], {"tag": "Stepped", "value": {"from": {"tag": "Ended", "value": UNIT}}})


if __name__ == "__main__":
    unittest.main()
