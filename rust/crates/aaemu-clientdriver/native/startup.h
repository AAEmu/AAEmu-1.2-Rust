#pragma once
#include <string>
namespace startup {
bool install();
bool install_at(unsigned char* crysystem);
void remove();
std::string info();
}
