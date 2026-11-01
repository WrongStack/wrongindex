package main

import (
	"fmt"
	u "example.com/gobasic/internal/util"
)

// Server holds config.
type Server struct{ port int }

// Start runs the server.
func (s *Server) Start() error {
	fmt.Println(u.Greet("go"))
	return nil
}

func main() {
	s := &Server{port: 8080}
	_ = s.Start()
}
