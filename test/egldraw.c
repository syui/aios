// EGL (surfaceless) と GLES2 で、画面なしに描いて読み返す (Mesa の virgl / llvmpipe のたしかめ。GPU の道の答えあわせ):
//   EGL_PLATFORM_SURFACELESS_MESA の display で GLES2 のコンテキストを作り、64x64 の FBO (RGBA8 の renderbuffer) を
//   青でぬって、まん中に赤い三角形を描き、glReadPixels で読み返す。すみは青、まん中は赤であること
//   (ドライバによらず同じ答え)。GL_RENDERER (virgl なら "virgl (...)"、なければ "llvmpipe (...)") も出す
// 1 行に ok NAME か FAIL NAME、おしまいに egldraw: N failed
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <string.h>

static int fails;

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static GLuint shader(GLenum type, const char *src) {
  GLuint s = glCreateShader(type);
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  GLint ok = 0;
  glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  if (!ok) {
    char log[512];
    glGetShaderInfoLog(s, sizeof log, NULL, log);
    printf("  shader: %s\n", log);
  }
  return s;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  PFNEGLGETPLATFORMDISPLAYEXTPROC get_display = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
  EGLDisplay dpy = get_display ? get_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL) : EGL_NO_DISPLAY;
  EGLint major = 0, minor = 0;
  int up = dpy != EGL_NO_DISPLAY && eglInitialize(dpy, &major, &minor);
  check("display (EGL_PLATFORM_SURFACELESS_MESA)", up);
  if (!up) {
    printf("egldraw: %d failed\n", fails);
    return 1;
  }
  eglBindAPI(EGL_OPENGL_ES_API);
  const EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
  EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
  int cur = ctx != EGL_NO_CONTEXT && eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
  check("context (GLES2, no config, no surface)", cur);
  if (!cur) {
    printf("egldraw: %d failed\n", fails);
    return 1;
  }
  printf("renderer: %s\n", (const char *)glGetString(GL_RENDERER));
  printf("version: %s\n", (const char *)glGetString(GL_VERSION));

  const int W = 64, H = 64;
  GLuint fb, rb;
  glGenFramebuffers(1, &fb);
  glBindFramebuffer(GL_FRAMEBUFFER, fb);
  glGenRenderbuffers(1, &rb);
  glBindRenderbuffer(GL_RENDERBUFFER, rb);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, W, H);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
  check("framebuffer complete", glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);

  GLuint prog = glCreateProgram();
  glAttachShader(prog, shader(GL_VERTEX_SHADER, "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }"));
  glAttachShader(prog, shader(GL_FRAGMENT_SHADER, "precision mediump float; void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }"));
  glBindAttribLocation(prog, 0, "p");
  glLinkProgram(prog);
  GLint linked = 0;
  glGetProgramiv(prog, GL_LINK_STATUS, &linked);
  check("program linked", linked);

  glViewport(0, 0, W, H);
  glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
  glClear(GL_COLOR_BUFFER_BIT);
  const GLfloat tri[] = {-0.5f, -0.5f, 0.5f, -0.5f, 0.0f, 0.5f};
  glUseProgram(prog);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, tri);
  glEnableVertexAttribArray(0);
  glDrawArrays(GL_TRIANGLES, 0, 3);
  glFinish();

  static unsigned char px[64 * 64 * 4];
  memset(px, 0x55, sizeof px);
  glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
  const unsigned char *corner = px, *center = px + ((H / 2) * W + W / 2) * 4;
  char name[200];
  snprintf(name, sizeof name, "corner is blue (%d,%d,%d,%d)", corner[0], corner[1], corner[2], corner[3]);
  check(name, corner[0] == 0 && corner[1] == 0 && corner[2] == 255 && corner[3] == 255);
  snprintf(name, sizeof name, "center is red (%d,%d,%d,%d)", center[0], center[1], center[2], center[3]);
  check(name, center[0] == 255 && center[1] == 0 && center[2] == 0 && center[3] == 255);
  check("no GL error", glGetError() == GL_NO_ERROR);

  eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
  eglDestroyContext(dpy, ctx);
  eglTerminate(dpy);
  printf("egldraw: %d failed\n", fails);
  return fails != 0;
}
