/* Pointer- and struct-heavy 3D math: the shape of a Jaguar engine inner loop. */
struct Vec { int x, y, z; };
struct Mat { int m[9]; };

int dot(struct Vec *a, struct Vec *b) {
    return a->x * b->x + a->y * b->y + a->z * b->z;
}

void cross(struct Vec *a, struct Vec *b, struct Vec *o) {
    o->x = a->y * b->z - a->z * b->y;
    o->y = a->z * b->x - a->x * b->z;
    o->z = a->x * b->y - a->y * b->x;
}

void xform(struct Mat *m, struct Vec *v, struct Vec *o) {
    o->x = (m->m[0] * v->x + m->m[1] * v->y + m->m[2] * v->z) >> 14;
    o->y = (m->m[3] * v->x + m->m[4] * v->y + m->m[5] * v->z) >> 14;
    o->z = (m->m[6] * v->x + m->m[7] * v->y + m->m[8] * v->z) >> 14;
}

int sum_local(void) {
    struct Vec s;
    s.x = 1; s.y = 2; s.z = 3;
    return s.x + s.y + s.z;
}

int walk(struct Vec *p, int n) {
    int acc = 0;
    for (int i = 0; i < n; i++) acc += p[i].x + p[i].z;
    return acc;
}
