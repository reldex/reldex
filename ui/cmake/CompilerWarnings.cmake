# AGENTS.md-level code quality applies here too: our own targets build
# warning-clean. This is applied only to targets we own (adapter/app/tests),
# never to Qt's own targets or to Corrosion's imported Rust libraries, so a
# warning inside Qt or moc-generated code is never something we are asked to
# fix.
#
# Deliberately no /WX or -Werror: moc/qmltyperegistrar-generated code is
# outside our control and an upstream Qt point release could introduce a new
# warning there. "Keep them warning-free" is enforced by reading the build
# log for OUR sources, not by failing the build on a warning we did not
# write.
function(reldex_enable_warnings target)
    if(MSVC)
        target_compile_options(${target} PRIVATE /W4)
    else()
        target_compile_options(${target} PRIVATE -Wall -Wextra)
    endif()
endfunction()
