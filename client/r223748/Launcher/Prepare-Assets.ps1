param([string]$Assets=(Join-Path $PSScriptRoot 'Assets'))
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Drawing
$source=[Drawing.Image]::FromFile((Join-Path $Assets 'art-original.png'))
$bitmap=New-Object Drawing.Bitmap(1200,800)
$g=[Drawing.Graphics]::FromImage($bitmap)
try {
 $g.InterpolationMode='HighQualityBicubic';$g.DrawImage($source,0,0,1200,800)
 $codec=[Drawing.Imaging.ImageCodecInfo]::GetImageEncoders()|Where-Object MimeType -eq 'image/jpeg'
 $parameters=New-Object Drawing.Imaging.EncoderParameters(1)
 $parameters.Param[0]=New-Object Drawing.Imaging.EncoderParameter([Drawing.Imaging.Encoder]::Quality,[long]88)
 $bitmap.Save((Join-Path $Assets 'banner.jpg'),$codec,$parameters)
}finally{$g.Dispose();$bitmap.Dispose();$source.Dispose()}
$source=[Drawing.Image]::FromFile((Join-Path $Assets 'icon-original.png'))
$images=@()
try {
 foreach($size in @(16,24,32,48,64,128,256)){
  $bitmap=New-Object Drawing.Bitmap($size,$size);$g=[Drawing.Graphics]::FromImage($bitmap)
  $memory=New-Object IO.MemoryStream
  try{$g.InterpolationMode='HighQualityBicubic';$g.DrawImage($source,0,0,$size,$size);$bitmap.Save($memory,[Drawing.Imaging.ImageFormat]::Png);$images+=,[pscustomobject]@{Size=$size;Bytes=$memory.ToArray()}}
  finally{$g.Dispose();$bitmap.Dispose();$memory.Dispose()}
 }
 $file=[IO.File]::Create((Join-Path $Assets 'AAEmu.ico'));$writer=New-Object IO.BinaryWriter($file)
 try{
  $writer.Write([uint16]0);$writer.Write([uint16]1);$writer.Write([uint16]$images.Count);$offset=6+16*$images.Count
  foreach($entry in $images){$dimension=if($entry.Size -eq 256){0}else{$entry.Size};$writer.Write([byte]$dimension);$writer.Write([byte]$dimension);$writer.Write([byte]0);$writer.Write([byte]0);$writer.Write([uint16]1);$writer.Write([uint16]32);$writer.Write([uint32]$entry.Bytes.Length);$writer.Write([uint32]$offset);$offset+=$entry.Bytes.Length}
  foreach($entry in $images){$writer.Write([byte[]]$entry.Bytes)}
 }finally{$writer.Dispose()}
}finally{$source.Dispose()}
Get-Item (Join-Path $Assets 'banner.jpg'),(Join-Path $Assets 'AAEmu.ico')|Select-Object Name,Length
