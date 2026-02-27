#!/bin/zsh

d=${0:a:h}
cd $d

t=`grep -Rs archlinux .|grep -v t.zsh|grep -v http|grep -v systemd|grep -v pacman`
n=`echo $t|wc -l`
for ((i=1;i<=$n;i++))
do
	tt=`echo $t|awk "NR==$i"`
	echo $tt
	f=`echo $tt|cut -d : -f 1`
	echo sed -i s/archlinux/aios/g $f
done

t=`grep -Rs "Arch Linux" .|grep -v t.zsh|grep -v http|grep -v systemd|grep -v pacman`
n=`echo $t|wc -l`
for ((i=1;i<=$n;i++))
do
	tt=`echo $t|awk "NR==$i"`
	echo $tt
	f=`echo $tt|cut -d : -f 1`
	echo sed -i "s/Arch\ Linux/aios/g" $f
done
