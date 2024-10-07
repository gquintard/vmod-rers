# vmod_rers (Regular Expression in RuSt)

This is a vmod for [varnish](http://varnish-cache.org/), allowing you to expand on what the native VCL regex can do.

Notably, it offers dynamic regex compilation backed by an LRU cache to speed processing up while limiting the memory footprint. On top of this, it supports named capturing groups which simplifies the handling of complex regex.

As usual, the full VCL API is described in [vmod.vcc](vmod.vcc), and if you need help, you can find me as `guillaume` on the [varnish discord server](https://discord.gg/EuwdvbZR6d).

## VCL Example

```bash
import rers;

sub vcl_init {
	new re_cache = rers.init(100);
}

sub vcl_recv {
	if (re_cache.is_match(req.url, "admin")) {
		return (pass);
	}
	if (re_cache.capture(req.http.authorization, "(\w+) (\w+)") {
		set req.http.auth_type = re_cache.group(1);
		set req.http.auth_credential = re_cache.group(2);
	}
}
```

## Requirements

You'll need:
- `cargo` (and the accompanying `rust` package)
- `python3`
- the `varnish` development libraries/headers

## Version matching

Depending on which `varnish` version tou are targeting, select the right `vmod_rers` version (available as git tags, and as [releases](https://github.com/gquintard/vmod_rers/releases)).

| vmod-rers | varnish |
|:----------|:-------:|
| v0.0.9    |   7.5   |
| v0.0.8    |   7.4   |
| v0.0.7    |   7.3   |
| v0.0.6    |   7.3   |
| v0.0.5    |   7.2   |
| v0.0.4    |   7.0   |
| v0.0.3    |   7.0   |
| v0.0.2    |   7.0   |
| v0.0.1    |   7.0   |

## Build and test

With `cargo` only:

```bash
cargo build --release
cargo test --release
```

The vmod file will be found at `target/release/libvmod_rs_template.so`.

Alternatively, if you have `jq` and `rst2man`, you can use `build.sh`

```bash
./build.sh [OUTDIR]
```

This will place the `so` file as well as the generated documentation in the `OUT` directory (or in the current directory if `OUT` wasn't specified).

## Packages

To avoid making a mess of your system, you probably should install your vmod as a proper package. This repository also offers different templates, and some quick recipes for different distributions.

### All platforms

First it's necessary to set the `VMOD_VERSION` (the version of this vmod) and `VARNISH_VERSION` (the Varnish version to build against) environment variables. It can be done manually, or using `cargo` and `jq`:
```bash
VMOD_VERSION=$(cargo metadata --no-deps --format-version 1 | jq '.packages[0].version' -r)
VARNISH_MINOR=$(cargo metadata --format-version 1 | jq -r '.packages[] | select(.name == "varnish-sys") | .metadata.libvarnishapi.version ')
VARNISH_PATCH=0
VARNISH_VERSION="$VARNISH_MINOR.$VARNISH_PATCH"

# or
VMOD_VERSION=0.0.1
VARNISH_VERSION=7.0.0
```

Then create the dist tarball, for example using `git archive`:

```bash
git archive --output=vmod_rs_template-$VMOD_VERSION.tar.gz --format=tar.gz HEAD
```

Then, follow distribution-specific instructions.

### Arch

```bash
# create a work directory
mkdir build
# copy the tarball and PKGBUILD file, substituing the variables we care about
cp vmod_rs_template-$VMOD_VERSION.tar.gz build
sed -e "s/@VMOD_VERSION@/$VMOD_VERSION/" -e "s/@VARNISH_VERSION@/$VARNISH_VERSION/" pkg/arch/PKGBUILD > build/PKGBUILD

# build
cd build
makepkg -rsf
```

Your package will be the file with the `.pkg.tar.zst` extension in `build/`

### Alpine

Alpine needs a bit of setup on the first time, but the [documentation](https://wiki.alpinelinux.org/wiki/Creating_an_Alpine_package) is excellent.

```bash
# install some packages, create a user, give it power and a key
apk add -q --no-progress --update tar alpine-sdk sudo
adduser -D builder
echo "builder ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers
addgroup builder abuild
su builder -c "abuild-keygen -nai"
```

Then, to actually build your package:

```bash
# create a work directory
mkdir build
# copy the tarball and PKGBUIL file, substituing the variables we care about
cp vmod_rs_template-$VMOD_VERSION.tar.gz build
sed -e "s/@VMOD_VERSION@/$VMOD_VERSION/" -e "s/@VARNISH_VERSION@/$VARNISH_VERSION/" pkg/arch/APKBUILD > build/APKBUILD

su builder -c "abuild checksum"
su builder -c "abuild -r"
```
