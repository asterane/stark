; STARK, a system for computer augmented design.
; Copyright (C) 2021 Matthew Rothlisberger

; STARK is licensed under the terms of the GNU Affero General Public
; License. See the top level LICENSE file for the license text.

; Find full copyright information in the top level COPYRIGHT file.

; <>

; scripts/rndr.sl

; Main loop for the rendering thread of STARK's full graphical mode.

; <>

(def rndr (fn []

(def c-input ())
(def m-input ())

(def alive #T)

(def line-col (vec-f32-make))
(vec-f32-push line-col (as-f32 0.0))
(vec-f32-push line-col (as-f32 0.0))
(vec-f32-push line-col (as-f32 0.0))

(def get-q-next (fn [q] (def out ()) (while (eq out ()) (set out (qrx q))) out))

;(bg-col engine (as-f32 0.0) (as-f32 0.0) (as-f32 0.0))
(redraw engine)

(print "prepared for render loop")

(while alive
       (set c-input (qrx cr-recv))
       (if (eq c-input :cx-dstr)
       (do
           (print "destroying render")
           (set alive #F))

       (if (eq c-input :cx-resz)
           (do (frame-size engine (get-q-next cr-recv) (get-q-next cr-recv)))

       (if (eq c-input :cx-rdrw)
           (do (redraw engine))

       ())))

       (set m-input (qrx mr-recv))
       (if (eq m-input :line)
           (do (def line (vec-f32-make))
               (vec-f32-push line (get-q-next mr-recv))
               (vec-f32-push line (get-q-next mr-recv))
               (vec-f32-push line (get-q-next mr-recv))
               (vec-f32-push line (get-q-next mr-recv))
               (new-line engine line line-col)
               (redraw engine))

       (if (eq m-input :line-col)
           (do (vec-f32-set line-col 0 (get-q-next mr-recv))
               (vec-f32-set line-col 1 (get-q-next mr-recv))
               (vec-f32-set line-col 2 (get-q-next mr-recv)))

       (if (eq m-input :back-col)
           (do (def r (get-q-next mr-recv))
               (def g (get-q-next mr-recv))
               (def b (get-q-next mr-recv))
               (bg-col engine r g b)
               (redraw engine))

       (if (eq m-input :clear)
           (do (clear engine)
               (redraw engine))

       ())))))

(print "render end")

))
