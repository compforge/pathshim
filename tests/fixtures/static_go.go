package main

import (
	"fmt"
	"os"
	"path/filepath"
)

func main() {
	output := "/project/go-output"
	if len(os.Args) > 1 {
		output = os.Args[1]
	}
	hostname, err := os.ReadFile("/etc/hostname")
	if err != nil {
		panic(err)
	}
	if err := os.MkdirAll(filepath.Dir(output), 0o755); err != nil {
		panic(err)
	}
	if err := os.WriteFile(output, hostname, 0o644); err != nil {
		panic(err)
	}
	written, err := os.ReadFile(output)
	if err != nil {
		panic(err)
	}
	fmt.Print(string(written))
}
