#!/usr/bin/env python3
"""Run the test-only corpus oracle without mixing test logs into its TSV.

Source scripts/env.sh first. Example:
  python3 scripts/query-oracle-dump.py gate1 --output /tmp/gate.tsv -- \
      --queries /tmp/queries.tsv --query-list /tmp/cases.txt /path/to/graph
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["walk", "gate1"])
    parser.add_argument("--output", required=True, type=Path)
    # Everything after -- belongs to the original corpus harness grammar.
    argv = sys.argv[1:]
    if "--" not in argv:
        parser.error("put corpus arguments after --")
    separator = argv.index("--")
    options = parser.parse_args(argv[:separator])
    corpus_args = argv[separator + 1:]
    if not corpus_args:
        parser.error("at least one corpus graph is required")
    environment = os.environ.copy()
    environment["TINE_QUERY_ORACLE_ARGS"] = json.dumps(corpus_args)
    environment["CARGO_INCREMENTAL"] = "0"
    root = Path(__file__).resolve().parent.parent
    output = options.output.resolve()
    with tempfile.TemporaryDirectory(prefix="tine-query-oracle-", dir=output.parent) as staging:
        temporary_output = Path(staging) / "result.tsv"
        environment["TINE_QUERY_ORACLE_OUTPUT"] = str(temporary_output)
        result = subprocess.run(
            ["rtk", "proxy", "cargo", "test", "-p", "tine-core", "--lib",
             f"query::oracle_{options.mode}::dump", "--", "--ignored", "--exact"],
            cwd=root, env=environment,
        )
        if result.returncode:
            return result.returncode
        if not temporary_output.is_file():
            parser.error("the selected oracle did not run; no TSV was produced")
        os.replace(temporary_output, output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
