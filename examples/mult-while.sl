; Project Euler Problem 1 Solution
; using Sail's while form

; run time, April 21 2021: ~0.07 seconds

(do
(def max 1000)
(def acc 0)
(def cur 1)
(while (not (= cur max))
       (if (= (mod cur 3) 0)
           (set acc (+ acc cur))
           (if (= (mod cur 5) 0)
               (set acc (+ acc cur))
               ()))
       (set cur (+ cur 1)))
acc
)