# OpenDocument spreadsheets

New workbooks save as `.ods` by default. Desktop Save and Save As use the file picker, and the
web app downloads the workbook. Choose Excel Workbook in Save As to write `.xlsx`.

An ODS saved by GridCraft contains standard ODF worksheets with cell values, merged cells and
common formulas that another spreadsheet app can calculate. It also carries an internal
workbook part to retain features such as cell styles, charts and formulas when reopened in
GridCraft. Another spreadsheet app sees the standard worksheet data. Its save can discard the
internal part, and GridCraft ignores the internal part if the worksheet XML has changed.

When opening a third-party ODS file, GridCraft imports sheets, values, supported dates and times,
and merged cells. Formula cells become their last saved results. Source formatting, charts and
images are not imported. GridCraft warns about these limits and asks for a new destination on
the first save, so it does not automatically overwrite the original.

```sh
gridcraft-cli info source.ods
gridcraft-cli convert source.ods imported.ods
gridcraft-cli convert source.xlsx workbook.ods
```

CLI import warnings are written to stderr.
