// Command govectors writes the Go reference vectors that jig-core's wire tests replay:
// strings.Fields over a byte corpus (behaviors/jigd-protocol.md \u00a7Tokenization) and
// strconv.ParseInt / strconv.Atoi over number spellings (\u00a7Number parsing).
//
// Run from the repository root:
//
//	go run ./crates/jig-testkit/gen/govectors tests/vectors
//
// The output is committed, so CI needs no Go to run the tests.
package main

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

type fieldsCase struct {
	In     string   `json:"in"`     // hex
	Fields []string `json:"fields"` // hex
}

type intCase struct {
	In    string `json:"in"` // hex
	Int64 *int64 `json:"int64"`
	Atoi  *int64 `json:"atoi"`
}

// pieces whose combinations exercise every separator, near misses, and invalid UTF-8
var pieces = []string{
	"", "a", "GET", "k", "0", "\x00", "\x1c", "\x1f", "\x7f", "\x80", "\x85", "\xa0", "\xff",
	"\t", "\n", "\v", "\f", "\r", " ",
	"\u0085", "\u00a0", "\u1680", "\u2000", "\u2005", "\u200a", "\u2028", "\u2029", "\u202f", "\u205f", "\u3000",
	"\u200b", "\ufeff", "\u0084", "\u00a1", "\u2027", "\u202a", "\u205e", "\u2060", "\u3001", "\u1681", "\u180e",
	"\u00e9", "\U0001F600",
	"\xc2", "\xe2\x80", "\xe2", "\xe1\x9a", "\xe3\x80", "\xc0\xa0", "\xe0\x80\xa0", "\xed\xa0\x80",
	"\xf0\x9f\x98", "\xf4\x90\x80\x80", "\xf8\x88\x80\x80\x80", "\xc2\xc2\x85", "\xe2\xe2\x80\x80",
}

func hx(s string) string { return hex.EncodeToString([]byte(s)) }

func fieldsVectors() []fieldsCase {
	seen := map[string]bool{}
	var out []fieldsCase
	add := func(s string) {
		if seen[s] {
			return
		}
		seen[s] = true
		f := strings.Fields(s)
		c := fieldsCase{In: hx(s), Fields: []string{}}
		for _, x := range f {
			c.Fields = append(c.Fields, hx(x))
		}
		out = append(out, c)
	}
	for _, a := range pieces {
		for _, b := range pieces {
			add(a + b)
			for _, c := range []string{"x", "\u3000"} {
				add(a + b + c)
				add(c + a + b)
			}
		}
	}
	r := rand.New(rand.NewSource(1))
	for i := 0; i < 4000; i++ {
		var sb strings.Builder
		n := r.Intn(8)
		for j := 0; j < n; j++ {
			sb.WriteString(pieces[r.Intn(len(pieces))])
		}
		add(sb.String())
	}
	for i := 0; i < 2000; i++ {
		b := make([]byte, r.Intn(12))
		for j := range b {
			b[j] = byte(r.Intn(256))
		}
		add(string(b))
	}
	return out
}

func intVectors() []intCase {
	ins := []string{
		"", "0", "1", "+0", "-0", "+", "-", "++1", "--1", "+-1", "00", "007", "-007", "+007",
		"1048576", "1048577", "-1", "2147483647", "2147483648", "-2147483648", "4294967296",
		"9223372036854775807", "9223372036854775808", "-9223372036854775808", "-9223372036854775809",
		"09223372036854775807", "18446744073709551615", "18446744073709551616", "99999999999999999999",
		"000000000000000000000000000001", "-000000000000000000000000000001", "1_0", "0x10", "0b1", "0o7",
		"1e3", " 1", "1 ", "1\n", "\t1", "\u0661", "\uff11", "1.0", "1,0", "abc", "1a", "a1", "-a", "+ 1",
		"123456789012345678", "1234567890123456789", "-123456789012345678", "-1234567890123456789",
	}
	var out []intCase
	for _, s := range ins {
		c := intCase{In: hx(s)}
		if v, err := strconv.ParseInt(s, 10, 64); err == nil {
			c.Int64 = &v
		}
		if v, err := strconv.Atoi(s); err == nil {
			w := int64(v)
			c.Atoi = &w
		}
		out = append(out, c)
	}
	return out
}

// one case per line, so diffs of regenerated vectors stay readable
func write[T any](dir, name string, cases []T) {
	var sb strings.Builder
	sb.WriteString("[\n")
	for i, c := range cases {
		b, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		sb.Write(b)
		if i+1 < len(cases) {
			sb.WriteByte(',')
		}
		sb.WriteByte('\n')
	}
	sb.WriteString("]\n")
	if err := os.WriteFile(filepath.Join(dir, name), []byte(sb.String()), 0o644); err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: govectors <out-dir>")
		os.Exit(2)
	}
	write(os.Args[1], "go_fields.json", fieldsVectors())
	write(os.Args[1], "go_parseint.json", intVectors())
}
