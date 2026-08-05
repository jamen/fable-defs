# fable-defs

[Fable: The Lost Chapters](https://en.wikipedia.org/wiki/Fable_(2004_video_game)) def compiler and Rust library

This is integrated into the [EgoCore](https://github.com/eeeeeAeoN/EgoCore) modding tool. A small CLI tool for Windows and Linux can be downloaded from the [releases](https://github.com/jamen/fable-defs/releases) page.

This is also a Rust library for using the def data types. I use it in my [openalbion](https://github.com/jamen/openalbion) engine project.

This project is experimental. Please report any bugs you find or suggest improvements.

## Usage

```
Usage: defc [-i <input>] [-o <output>] [--version]

Fable def compiler

Options:
  -i, --input       input directory of .def, .tpl, and .h files
  -o, --output      output directory for .bin files
  --version         print version
  --help, help      display usage information
```

## License

[Zlib license](./LICENSE)
