import argparse
import json

from report import build_report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("input")
    parser.add_argument("--output")
    args = parser.parse_args()

    with open(args.input, encoding="utf-8") as handle:
        report = build_report(json.load(handle))

    if args.output:
        with open(args.output, "w", encoding="utf-8") as handle:
            handle.write(report)
    else:
        print(report, end="")


if __name__ == "__main__":
    main()
