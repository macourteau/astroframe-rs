Local-only reference material. Nothing in this directory is committed except this file.

Fetch and convert the XISF 1.0 specification in one step:

    ./tools/fetch-xisf-spec.sh

It downloads the compiled HTML, prints the compiler stamp that identifies the
version, and writes `xisf-1.0-spec.md` beside it. `tools/xisf-spec-to-md.py`
does the conversion alone if the HTML is already local.

The design cites the specification by section number, so the compiled HTML at
the URL below is the authority: a source that lags it would point every citation
somewhere subtly wrong.

Source: https://pixinsight.com/doc/docs/XISF-1.0-spec/XISF-1.0-spec.html
Copyright Pleiades Astrophoto. Consult it locally; do not commit or redistribute
the converted copy.

FITS Standard 4.0: https://fits.gsfc.nasa.gov/fits_standard.html
