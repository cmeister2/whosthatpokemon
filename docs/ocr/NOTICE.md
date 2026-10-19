# Local OCR assets

The demo serves these files itself. Card pictures and OCR input are processed in
the visitor's browser; no image is submitted to an OCR service.

- Tesseract.js 7.0.0: <https://github.com/naptha/tesseract.js>, Apache-2.0;
  see `TESSERACT-LICENSE.md` and `worker.min.js.LICENSE.txt`.
- Tesseract.js Core 7.0.0: <https://github.com/naptha/tesseract.js-core>, Apache-2.0;
  see `CORE-LICENSE`.
- English trained data from `@tesseract.js-data/eng` 1.0.0,
  `4.0.0/eng.traineddata.gz`: <https://github.com/naptha/tessdata>.
  The package declares MIT; Tesseract language data is made available under
  Apache-2.0 by <https://github.com/naptha/tessdata> and
  <https://github.com/tesseract-ocr/tessdata>; see `LANGUAGE-LICENSE` and
  `LANGUAGE-DATA-LICENSE`.

The minified upstream bundles retain their own third-party license notices.
