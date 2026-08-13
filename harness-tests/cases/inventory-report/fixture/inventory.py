import json
import sys


def main():
    with open(sys.argv[1], encoding="utf-8") as handle:
        products = json.load(handle)
    print(json.dumps(products, indent=2))


if __name__ == "__main__":
    main()
