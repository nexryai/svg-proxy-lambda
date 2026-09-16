package main

import (
	"fmt"
	"image"
	"image/png"
	"log"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/nexryai/archer"
	"github.com/srwiley/oksvg"
	"github.com/srwiley/rasterx"
)

const (
	defaultSize = 512

	maxDimension = 4096
	maxPixels    = 4096 * 4096

	maxSVGBytes        = 5 * 1024 * 1024
	fetchTimeoutSecond = 10
)

func main() {
	mux := http.NewServeMux()

	mux.HandleFunc("/", handleConvert)
	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok\n"))
	})

	server := &http.Server{
		Addr:              ":8080",
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	log.Fatal(server.ListenAndServe())
}

func handleConvert(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		w.Header().Set("Allow", http.MethodGet)
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	target := strings.TrimSpace(r.URL.Query().Get("url"))
	if target == "" {
		http.Error(w, "missing url parameter", http.StatusBadRequest)
		return
	}

	size, err := parseOptionalPositiveInt(r.URL.Query().Get("size"))
	if err != nil {
		http.Error(w, "invalid size parameter", http.StatusBadRequest)
		return
	}

	width, err := parseOptionalPositiveInt(r.URL.Query().Get("width"))
	if err != nil {
		http.Error(w, "invalid width parameter", http.StatusBadRequest)
		return
	}

	height, err := parseOptionalPositiveInt(r.URL.Query().Get("height"))
	if err != nil {
		http.Error(w, "invalid height parameter", http.StatusBadRequest)
		return
	}

	// ?size=512 は 512x512 の省略記法
	if size != 0 {
		if width != 0 || height != 0 {
			http.Error(
				w,
				"size cannot be combined with width or height",
				http.StatusBadRequest,
			)
			return
		}

		width = size
		height = size
	}

	req, err := http.NewRequestWithContext(
		r.Context(),
		http.MethodGet,
		target,
		nil,
	)
	if err != nil {
		http.Error(w, "invalid url", http.StatusBadRequest)
		return
	}

	req.Header.Set(
		"Accept",
		"image/svg+xml,application/xml;q=0.9,text/xml;q=0.8",
	)
	req.Header.Set("User-Agent", "svg-png-proxy/1.0")

	secureRequest := archer.SecureRequest{
		Request:     req,
		TimeoutSecs: fetchTimeoutSecond,
		MaxSize:     maxSVGBytes,
	}

	resp, err := secureRequest.Send()
	if err != nil {
		log.Printf("failed to fetch %q: %v", target, err)
		http.Error(w, "failed to fetch svg", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		http.Error(
			w,
			fmt.Sprintf("upstream returned HTTP %d", resp.StatusCode),
			http.StatusBadGateway,
		)
		return
	}

	icon, err := oksvg.ReadIconStream(resp.Body)
	if err != nil {
		http.Error(
			w,
			"invalid or unsupported svg",
			http.StatusUnprocessableEntity,
		)
		return
	}

	if icon.ViewBox.W <= 0 || icon.ViewBox.H <= 0 {
		http.Error(
			w,
			"svg has no valid size or viewBox",
			http.StatusUnprocessableEntity,
		)
		return
	}

	width, height, err = resolveDimensions(
		width,
		height,
		icon.ViewBox.W,
		icon.ViewBox.H,
	)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	img := image.NewRGBA(
		image.Rect(0, 0, width, height),
	)

	scanner := rasterx.NewScannerGV(
		width,
		height,
		img,
		img.Bounds(),
	)

	raster := rasterx.NewDasher(
		width,
		height,
		scanner,
	)

	// oksvg の ViewBox を指定されたPNGサイズへ変換
	icon.SetTarget(
		0,
		0,
		float64(width),
		float64(height),
	)

	icon.Draw(raster, 1.0)

	// Lambda Response Streamingとしてクライアントへ転送する。
	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.WriteHeader(http.StatusOK)

	if err := png.Encode(w, img); err != nil {
		// この時点では既にレスポンスヘッダを送っているので
		// HTTPステータスの変更はできない。
		log.Printf("png encode failed: %v", err)
	}
}

func parseOptionalPositiveInt(value string) (int, error) {
	if value == "" {
		return 0, nil
	}

	n, err := strconv.Atoi(value)
	if err != nil || n <= 0 {
		return 0, fmt.Errorf("must be a positive integer")
	}

	return n, nil
}

func resolveDimensions(
	width int,
	height int,
	sourceWidth float64,
	sourceHeight float64,
) (int, int, error) {
	switch {
	case width == 0 && height == 0:
		// デフォルトでは横幅512px、アスペクト比維持
		width = defaultSize
		height = max(
			1,
			int(float64(width)*sourceHeight/sourceWidth+0.5),
		)

	case width == 0:
		// heightだけならアスペクト比維持
		width = max(
			1,
			int(float64(height)*sourceWidth/sourceHeight+0.5),
		)

	case height == 0:
		// widthだけならアスペクト比維持
		height = max(
			1,
			int(float64(width)*sourceHeight/sourceWidth+0.5),
		)
	}

	if width > maxDimension || height > maxDimension {
		return 0, 0, fmt.Errorf(
			"dimensions must not exceed %dx%d",
			maxDimension,
			maxDimension,
		)
	}

	if int64(width)*int64(height) > int64(maxPixels) {
		return 0, 0, fmt.Errorf("image contains too many pixels")
	}

	return width, height, nil
}
