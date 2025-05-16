# flatpak-rs

This is a toy to validate the API of the composefs-rs project for various
users.  It's currently capable of listing, searching and pulling apps from OCI
flatpak repositories.

Support for running is planned, but raises some interesting questions about how
we mount the composefs.  Access to mounting erofs and using FUSE fd passthrough
are currently only available to the (real) root user on the latest kernel
versions.  composefs (in C) has a non-passthrough FUSE backend that we might be
able to use...
