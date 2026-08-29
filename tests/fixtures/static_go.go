package main

import (
	"fmt"
	"os"
)

func main() {
	hostname, err := os.ReadFile("/etc/hostname")
	if err != nil {
		panic(err)
	}
	if err := os.MkdirAll("/project", 0o755); err != nil {
		panic(err)
	}
	if err := os.WriteFile("/project/go-output", hostname, 0o644); err != nil {
		panic(err)
	}
	written, err := os.ReadFile("/project/go-output")
	if err != nil {
		panic(err)
	}
	fmt.Print(string(written))
}
