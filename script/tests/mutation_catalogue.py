"""The catalogue loader reads both forms as ckdev-mutate does and names each row's file."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import mutation_catalogue as catalogue  # noqa: E402  (needs the script directory on the path)

ROW = '[[control]]\nid = "{id}"\nguards = "{guards}"\nplatforms = ["linux"]\n'


def row(identity, guards="g"):
    return ROW.format(id=identity, guards=guards)


class CatalogueLoading(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def test_a_single_file_is_loaded_with_each_rows_id_line(self):
        self.write("mutations.toml", "# header\n\n" + row("first") + "\n" + row("second"))
        rows, sources = catalogue.load(self.root)
        self.assertEqual([r["id"] for r in rows], ["first", "second"])
        self.assertEqual(str(sources["first"]), "mutations.toml:4")
        self.assertEqual(str(sources["second"]), "mutations.toml:9")

    def test_the_directory_reads_visible_toml_fragments_in_byte_order(self):
        self.write("mutations.d/b.toml", row("from-b"))
        self.write("mutations.d/Z.toml", row("from-upper-z"))
        self.write("mutations.d/a.toml", row("from-a") + "\n" + row("from-a-2"))
        # The runner ignores hidden files, other extensions and subdirectories.
        self.write("mutations.d/.hidden.toml", row("hidden"))
        self.write("mutations.d/notes.md", row("markdown"))
        self.write("mutations.d/nested/c.toml", row("nested"))
        rows, sources = catalogue.load(self.root)
        self.assertEqual([r["id"] for r in rows], ["from-upper-z", "from-a", "from-a-2", "from-b"])
        self.assertEqual(sources["from-a-2"], catalogue.Source("mutations.d/a.toml", 7))
        self.assertEqual(sources["from-b"].file, "mutations.d/b.toml")

    def test_both_forms_at_once_are_refused(self):
        self.write("mutations.toml", row("single"))
        self.write("mutations.d/a.toml", "")
        with self.assertRaisesRegex(ValueError, "both mutations.toml and mutations.d/"):
            catalogue.load(self.root)

    def test_an_explicit_path_selects_one_form_relative_to_the_root(self):
        self.write("proofs/a.toml", row("explicit"))
        self.write("mutations.toml", row("default"))
        rows, sources = catalogue.load(self.root, Path("proofs"))
        self.assertEqual([r["id"] for r in rows], ["explicit"])
        self.assertEqual(str(sources["explicit"]), "proofs/a.toml:2")

    def test_a_missing_catalogue_is_an_error(self):
        with self.assertRaisesRegex(ValueError, "no mutation catalogue at mutations.toml"):
            catalogue.load(self.root)

    def test_duplicate_ids_across_fragments_name_every_definition(self):
        self.write("mutations.d/a.toml", row("twice"))
        self.write("mutations.d/b.toml", "\n" + row("twice"))
        with self.assertRaisesRegex(ValueError, r"duplicate catalogue ID twice: defined at "
                                                r"mutations.d/a.toml:2, mutations.d/b.toml:3"):
            catalogue.load(self.root)

    def test_duplicate_ids_in_one_file_are_refused(self):
        self.write("mutations.toml", row("twice") + row("twice"))
        with self.assertRaisesRegex(ValueError, "duplicate catalogue ID twice"):
            catalogue.load(self.root)

    def test_a_parse_error_names_its_fragment(self):
        self.write("mutations.d/broken.toml", "[[control]\n")
        with self.assertRaisesRegex(ValueError, "^mutations.d/broken.toml: "):
            catalogue.load(self.root)


class RevisionComparison(unittest.TestCase):
    """Moving rows between files or forms must leave every row's fields unchanged."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git("init", "--quiet")
        (self.root / "mutations.toml").write_text(row("one") + "\n" + row("two", "second guard"))
        self.git("add", "mutations.toml")
        self.git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "--quiet", "-m", "single")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True)

    def split(self, second):
        (self.root / "mutations.toml").unlink()
        (self.root / "mutations.d").mkdir()
        (self.root / "mutations.d/a.toml").write_text("# first area\n\n" + row("one"))
        (self.root / "mutations.d/b.toml").write_text(second)

    def compare(self):
        return catalogue.differences(catalogue.load_at_revision(self.root, "HEAD"), catalogue.load(self.root))

    def test_a_pure_move_into_fragments_has_no_differences(self):
        self.split(row("two", "second guard"))
        self.assertEqual(self.compare(), [])

    def test_a_changed_field_names_the_row_field_and_both_sources(self):
        self.split(row("two", "edited guard"))
        self.assertEqual(self.compare(), ["two: changed guards (was mutations.toml:7, now mutations.d/b.toml:2)"])

    def test_dropped_and_added_rows_are_reported(self):
        self.split(row("three", "second guard"))
        self.assertEqual(self.compare(), ["two: removed (was mutations.toml:7)",
                                          "three: added at mutations.d/b.toml:2"])

    def test_a_committed_directory_is_read_back_from_git(self):
        self.split(row("two", "second guard"))
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "--quiet", "-m", "split")
        rows, sources = catalogue.load_at_revision(self.root, "HEAD")
        self.assertEqual([r["id"] for r in rows], ["one", "two"])
        self.assertEqual(str(sources["one"]), "mutations.d/a.toml:4")
        self.assertEqual(rows, catalogue.load_at_revision(self.root, "HEAD~1")[0])


if __name__ == "__main__":
    unittest.main(verbosity=2)
