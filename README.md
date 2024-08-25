# Encode all video files in directory

1. Write configuration file: `~/.config/batch-av1/config.toml`
2. Run `batch-av1 all /path/to/video/dirctory 93` (93 means target VMAF score)

## Depends on my patched VMAF and ab-av1

Unfortunately, this script depends on my patched VMAF and ab-av1, currently need to be installed manually below:

- https://github.com/amachang/vmaf
- https://github.com/amachang/ab-av1

