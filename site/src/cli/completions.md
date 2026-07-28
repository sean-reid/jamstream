# jamstream completions

Print shell completions for jamstream.

```text
Usage: jamstream completions <SHELL>
```

`SHELL` is one of `bash`, `zsh`, `fish`, `powershell`, or `elvish`.

Installed through Homebrew, completions are set up for you. Installed any
other way, add one line:

```console
# zsh (~/.zshrc)
$ source <(jamstream completions zsh)

# bash (~/.bashrc)
$ source <(jamstream completions bash)

# fish
$ jamstream completions fish > ~/.config/fish/completions/jamstream.fish
```
