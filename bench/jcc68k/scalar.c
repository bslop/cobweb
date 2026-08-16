/* Scalar control flow, globals, arrays: general game-logic shape. */
int tab[64];
int gcount;

int clampi(int v, int lo, int hi) {
    if (v < lo) return lo;
    if (v > hi) return hi;
    return v;
}

int sumtab(int n) {
    int s = 0;
    for (int i = 0; i < n; i++) s += tab[i];
    return s;
}

int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

int classify(int x) {
    switch (x) {
        case 0: return 10;
        case 1: return 20;
        case 2: return 30;
        default: return 40;
    }
}

void bump(void) { gcount = gcount + 1; }

int strlen_(const char *s) {
    int n = 0;
    while (s[n]) n++;
    return n;
}
