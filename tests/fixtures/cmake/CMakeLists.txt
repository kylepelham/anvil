cmake_minimum_required(VERSION 3.16)
project(calc C)

option(CALC_FAST "Use the fast path" OFF)

add_library(calc STATIC lib/calc.c)
target_include_directories(calc PUBLIC lib)
if(CALC_FAST)
  target_compile_definitions(calc PRIVATE CALC_FAST)
endif()

add_executable(calc_app main.c)
target_link_libraries(calc_app PRIVATE calc)

enable_testing()
add_test(NAME calc_runs COMMAND calc_app 2 3)
set_tests_properties(calc_runs PROPERTIES PASS_REGULAR_EXPRESSION "sum 5")
