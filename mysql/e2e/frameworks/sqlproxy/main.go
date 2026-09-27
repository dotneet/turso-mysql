// sqlproxy sits between one app and one MySQL server and writes every
// statement the app sends, with the server's answer, to a JSON-lines file.
//
// Neither turso-mysql nor the frameworks log refused SQL in one common form,
// so the harness reads it off the wire instead. Both servers require TLS, so
// the proxy relays the MySQL greeting and the client's SSLRequest unchanged,
// then terminates TLS on each side with the harness's test certificate. It
// never rewrites a packet; it only reads the command byte of each client
// command and the first byte of the server's answer.
package main

import (
	"crypto/tls"
	"crypto/x509"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

const (
	clientSSL             = 0x00000800
	clientQueryAttributes = 0x08000000

	comQuit             = 0x01
	comInitDB           = 0x02
	comQuery            = 0x03
	comChangeUser       = 0x11
	comStmtPrepare      = 0x16
	comStmtExecute      = 0x17
	comStmtSendLongData = 0x18
	comStmtClose        = 0x19
)

type entry struct {
	Time     string `json:"time"`
	Conn     int64  `json:"conn"`
	Command  string `json:"command"`
	SQL      string `json:"sql"`
	OK       bool   `json:"ok"`
	Code     int    `json:"code,omitempty"`
	SQLState string `json:"sqlstate,omitempty"`
	Message  string `json:"message,omitempty"`
}

type logWriter struct {
	mu  sync.Mutex
	enc *json.Encoder
	f   *os.File
}

func (w *logWriter) write(e entry) {
	w.mu.Lock()
	defer w.mu.Unlock()
	e.Time = time.Now().UTC().Format(time.RFC3339Nano)
	if err := w.enc.Encode(e); err != nil {
		log.Printf("write log: %v", err)
	}
	_ = w.f.Sync()
}

func main() {
	listen := flag.String("listen", "127.0.0.1:3306", "address the app connects to")
	upstream := flag.String("upstream", "", "host:port of the MySQL server")
	upstreamName := flag.String("upstream-name", "", "TLS server name of the upstream (defaults to its host)")
	cert := flag.String("cert", "", "certificate chain presented to the app")
	key := flag.String("key", "", "private key presented to the app")
	ca := flag.String("ca", "", "CA that signed the upstream certificate")
	logPath := flag.String("log", "", "JSON-lines statement log")
	readyPath := flag.String("ready", "", "file created once the proxy listens")
	flag.Parse()
	if *upstream == "" || *cert == "" || *key == "" || *ca == "" || *logPath == "" {
		log.Fatal("-upstream, -cert, -key, -ca and -log are required")
	}
	serverName := *upstreamName
	if serverName == "" {
		host, _, err := net.SplitHostPort(*upstream)
		if err != nil {
			log.Fatalf("upstream: %v", err)
		}
		serverName = host
	}

	pair, err := tls.LoadX509KeyPair(*cert, *key)
	if err != nil {
		log.Fatalf("load certificate: %v", err)
	}
	caPEM, err := os.ReadFile(*ca)
	if err != nil {
		log.Fatalf("read CA: %v", err)
	}
	roots := x509Pool(caPEM)
	serverTLS := &tls.Config{Certificates: []tls.Certificate{pair}, MinVersion: tls.VersionTLS12}
	clientTLS := &tls.Config{RootCAs: roots, ServerName: serverName, MinVersion: tls.VersionTLS12}

	f, err := os.OpenFile(*logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		log.Fatalf("open log: %v", err)
	}
	out := &logWriter{enc: json.NewEncoder(f), f: f}
	out.enc.SetEscapeHTML(false)

	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		log.Fatalf("listen: %v", err)
	}
	if *readyPath != "" {
		if err := os.WriteFile(*readyPath, nil, 0o644); err != nil {
			log.Fatalf("write ready file: %v", err)
		}
	}
	var connID atomic.Int64
	for {
		client, err := ln.Accept()
		if err != nil {
			log.Fatalf("accept: %v", err)
		}
		id := connID.Add(1)
		go func() {
			if err := relay(id, client, *upstream, serverTLS, clientTLS, out); err != nil && !errors.Is(err, io.EOF) {
				log.Printf("conn %d: %v", id, err)
			}
		}()
	}
}

func relay(id int64, client net.Conn, upstream string, serverTLS, clientTLS *tls.Config, out *logWriter) error {
	defer client.Close()
	server, err := net.Dial("tcp", upstream)
	if err != nil {
		return fmt.Errorf("dial upstream: %w", err)
	}
	defer server.Close()

	greeting, err := readPacket(server)
	if err != nil {
		return fmt.Errorf("read greeting: %w", err)
	}
	if err := writePacket(client, greeting); err != nil {
		return err
	}
	if len(greeting.payload) > 0 && greeting.payload[0] == 0xff {
		return nil
	}
	first, err := readPacket(client)
	if err != nil {
		return fmt.Errorf("read client handshake: %w", err)
	}
	if err := writePacket(server, first); err != nil {
		return err
	}
	var c, s net.Conn = client, server
	if len(first.payload) >= 4 && binary.LittleEndian.Uint32(first.payload[:4])&clientSSL != 0 {
		ts := tls.Server(client, serverTLS)
		if err := ts.Handshake(); err != nil {
			return fmt.Errorf("client TLS: %w", err)
		}
		tc := tls.Client(server, clientTLS)
		if err := tc.Handshake(); err != nil {
			return fmt.Errorf("upstream TLS: %w", err)
		}
		c, s = ts, tc
	}

	st := &connState{
		id:             id,
		out:            out,
		authenticating: true,
		prepared:       map[uint32]string{},
		serverCaps:     greetingCapabilities(greeting.payload),
	}
	if len(first.payload) >= 4 {
		st.clientCaps = binary.LittleEndian.Uint32(first.payload[:4])
	}
	errc := make(chan error, 2)
	go func() { errc <- st.clientToServer(c, s) }()
	go func() { errc <- st.serverToClient(s, c) }()
	err = <-errc
	client.Close()
	server.Close()
	<-errc
	return err
}

type pending struct {
	command string
	sql     string
	cmd     byte
}

type connState struct {
	id             int64
	out            *logWriter
	mu             sync.Mutex
	authenticating bool
	waiting        *pending
	prepared       map[uint32]string
	serverCaps     uint32
	clientCaps     uint32
}

func (st *connState) clientToServer(c, s net.Conn) error {
	for {
		p, err := readPacket(c)
		if err != nil {
			return err
		}
		st.noteCommand(p)
		if err := writePacket(s, p); err != nil {
			return err
		}
	}
}

func (st *connState) noteCommand(p packet) {
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.authenticating || p.seq != 0 || len(p.payload) == 0 {
		return
	}
	body := p.payload[1:]
	switch cmd := p.payload[0]; cmd {
	case comQuery:
		if st.serverCaps&st.clientCaps&clientQueryAttributes != 0 {
			body = stripQueryAttributes(body)
		}
		st.waiting = &pending{command: "query", sql: string(body), cmd: cmd}
	case comStmtPrepare:
		st.waiting = &pending{command: "prepare", sql: string(body), cmd: cmd}
	case comStmtExecute:
		sql := "<unknown statement>"
		if len(body) >= 4 {
			sql = st.prepared[binary.LittleEndian.Uint32(body[:4])]
		}
		st.waiting = &pending{command: "execute", sql: sql, cmd: cmd}
	case comInitDB:
		st.waiting = &pending{command: "init_db", sql: "USE `" + string(body) + "`", cmd: cmd}
	case comChangeUser:
		st.authenticating = true
		st.waiting = nil
	case comQuit, comStmtClose, comStmtSendLongData:
		st.waiting = nil
	default:
		st.waiting = &pending{command: fmt.Sprintf("command_0x%02x", cmd), cmd: cmd}
	}
}

func (st *connState) serverToClient(s, c net.Conn) error {
	for {
		p, err := readPacket(s)
		if err != nil {
			return err
		}
		st.noteAnswer(p)
		if err := writePacket(c, p); err != nil {
			return err
		}
	}
}

func (st *connState) noteAnswer(p packet) {
	st.mu.Lock()
	defer st.mu.Unlock()
	if len(p.payload) == 0 {
		return
	}
	if st.authenticating {
		switch p.payload[0] {
		case 0x00:
			st.authenticating = false
		case 0xff:
			code, state, msg := parseErr(p.payload)
			st.out.write(entry{Conn: st.id, Command: "authenticate", Code: code, SQLState: state, Message: msg})
		}
		return
	}
	w := st.waiting
	if w == nil {
		return
	}
	st.waiting = nil
	e := entry{Conn: st.id, Command: w.command, SQL: w.sql, OK: p.payload[0] != 0xff}
	if !e.OK {
		e.Code, e.SQLState, e.Message = parseErr(p.payload)
	} else if w.cmd == comStmtPrepare && len(p.payload) >= 5 {
		st.prepared[binary.LittleEndian.Uint32(p.payload[1:5])] = w.sql
	}
	st.out.write(e)
}

// stripQueryAttributes drops the attribute header a COM_QUERY carries once
// CLIENT_QUERY_ATTRIBUTES is negotiated, so the same statement logs the same
// text against both servers. Only the attribute-free form is stripped; a
// query that really carries attributes is logged raw.
func stripQueryAttributes(body []byte) []byte {
	if len(body) >= 2 && body[0] == 0 && body[1] == 1 {
		return body[2:]
	}
	return body
}

// greetingCapabilities reads the capability flags of a HandshakeV10 packet.
func greetingCapabilities(payload []byte) uint32 {
	if len(payload) < 1 || payload[0] != 10 {
		return 0
	}
	i := 1
	for i < len(payload) && payload[i] != 0 {
		i++
	}
	i += 1 + 4 + 8 + 1
	if i+2 > len(payload) {
		return 0
	}
	caps := uint32(binary.LittleEndian.Uint16(payload[i : i+2]))
	i += 2 + 1 + 2
	if i+2 <= len(payload) {
		caps |= uint32(binary.LittleEndian.Uint16(payload[i:i+2])) << 16
	}
	return caps
}

func parseErr(payload []byte) (int, string, string) {
	if len(payload) < 3 {
		return 0, "", ""
	}
	code := int(binary.LittleEndian.Uint16(payload[1:3]))
	rest := payload[3:]
	state := ""
	if len(rest) >= 6 && rest[0] == '#' {
		state = string(rest[1:6])
		rest = rest[6:]
	}
	return code, state, string(rest)
}

type frame struct {
	seq  byte
	size int
}

type packet struct {
	seq     byte
	payload []byte
	frames  []frame
}

// readPacket joins a payload split across 16 MiB frames so the command byte
// and error header are always read from the logical packet.
func readPacket(r io.Reader) (packet, error) {
	var header [4]byte
	var p packet
	for {
		if _, err := io.ReadFull(r, header[:]); err != nil {
			return p, err
		}
		n := int(header[0]) | int(header[1])<<8 | int(header[2])<<16
		buf := make([]byte, n)
		if _, err := io.ReadFull(r, buf); err != nil {
			return p, err
		}
		if p.payload == nil {
			p.seq = header[3]
		}
		p.payload = append(p.payload, buf...)
		p.frames = append(p.frames, frame{seq: header[3], size: n})
		if n < 0xffffff {
			return p, nil
		}
	}
}

func writePacket(w io.Writer, p packet) error {
	offset := 0
	for _, f := range p.frames {
		header := []byte{byte(f.size), byte(f.size >> 8), byte(f.size >> 16), f.seq}
		if _, err := w.Write(append(header, p.payload[offset:offset+f.size]...)); err != nil {
			return err
		}
		offset += f.size
	}
	return nil
}

func x509Pool(pemBytes []byte) *x509.CertPool {
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(pemBytes) {
		log.Fatal("CA file holds no certificate")
	}
	return pool
}
