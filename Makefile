.PHONY: install run test check

install:
	python3 -m venv .venv
	.venv/bin/python -m pip install -e .

run:
	.venv/bin/photoscanner

test:
	.venv/bin/python -m unittest discover -s tests -v

check: test
	.venv/bin/python -m compileall -q src tests
	.venv/bin/photoscanner --help >/dev/null
