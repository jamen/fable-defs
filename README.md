# fable-defs

Fable def compiler and Rust library

The modding tool [EgoCore](https://github.com/eeeeeAeoN/EgoCore) provides a GUI for the compiler, among many other features. A small CLI tool for Windows and Linux is available on the [releases](https://github.com/jamen/fable-defs/releases) page.

This is also a Rust library for working with the def data types. I use it in my [openalbion](https://github.com/jamen/openalbion) engine recreation project.

This project is experimental. Please report any bugs you find or suggest improvements.

## Usage

```
Usage: defc [--] <source> <output>

Fable def compiler

Positional Arguments:
  source            input directory containing .def, .tpl, and .h files
  output            output directory for .bin files
```

## License

[Zlib license](./LICENSE)
