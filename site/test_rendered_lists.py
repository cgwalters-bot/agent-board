from html.parser import HTMLParser
from pathlib import Path
import unittest
from xml.etree.ElementTree import TreeBuilder


class ListStructure(HTMLParser):
    """Keep list and block nesting and text; ignore theme and highlighting markup."""

    tags = {"ol", "ul", "li", "p", "pre"}

    def __init__(self, page):
        super().__init__()
        self.tree = TreeBuilder()
        self.tree.start("page", {})
        self.feed((Path(__file__).parent / "dist/docs" / page / "index.html").read_text())
        self.close()
        self.tree.end("page")
        self.root = self.tree.close()

    def handle_starttag(self, tag, attrs):
        if tag in self.tags:
            self.tree.start(tag, {})

    def handle_endtag(self, tag):
        if tag in self.tags:
            self.tree.end(tag)

    def handle_data(self, data):
        self.tree.data(data)


def text(element):
    return " ".join("".join(element.itertext()).split())


class RenderedLists(unittest.TestCase):
    def test_pass_stages(self):
        stages, = (ol.findall("li") for ol in ListStructure("pass").root.iter("ol")
                   if ol.find("li") is not None and text(ol[0]).startswith("Refresh usage"))
        self.assertIn("4b. Read-only promotion visibility", text(stages[3]))
        self.assertTrue(text(stages[4]).startswith("Derive Status"))
        self.assertEqual(len(stages[4].findall("ul/li")), 5)
        self.assertTrue(text(stages[5]).startswith("With cost,"))

    def test_setup_examples(self):
        step, = (li for li in ListStructure("setup").root.findall(".//ol/li")
                 if text(li).startswith("Preview the first live pass"))
        config, commands = step.findall("pre")
        self.assertIn('"dispatchRepo"', text(config))
        self.assertIn("agent-board snapshot create", text(commands))

    def test_forge_proof(self):
        proof, = (li for li in ListStructure("forges").root.findall(".//ul/li")
                  if text(li).startswith("API proof, in CI."))
        self.assertIn("It proves nothing about agents", text(proof))
