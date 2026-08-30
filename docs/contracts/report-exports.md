# User-selected report exports — living contract

Scope: diagnostic and graph-verification JSON saved to a destination explicitly
chosen by the user. These files are exports, not graph authority, and do not
need graph mutation conflict or baseline-CAS semantics.

An export stages complete bytes in a unique same-directory temporary file,
flushes the file, atomically replaces the selected destination, and flushes the
parent directory before reporting success. A crash may therefore leave the old
complete report or the new complete report, never a destination truncated by
an in-place write. Temporary files are removed on reported failure when
possible.

Unsupported directory flushing follows the shared atomic-write policy; real
I/O and capacity errors are returned to the UI. The destination remains the
user's explicit authority boundary—this contract does not authorize saving a
report anywhere the chooser did not select.
